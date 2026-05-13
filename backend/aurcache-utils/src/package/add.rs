use crate::git::checkout::checkout_repo_ref;
use alpm_srcinfo::SourceInfoV1;
use anyhow::{anyhow, bail};
use aurcache_db::dependencies;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::{Dependencies, Packages};
use aurcache_db::{builds, packages};
use aurcache_types::builder::{Action, BuildStates};
use pacman_mirrors::platforms::{Platform, Platforms};
use sea_orm::QueryFilter;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait,
    PaginatorTrait, TransactionTrait, TryIntoModel,
};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::tempdir;
use tokio::sync::broadcast::Sender;

type RecResult = std::result::Result<(), anyhow::Error>;
type AddRecursiveFut<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = RecResult> + Send + 'a>>;

struct InsertPackageParams<'a> {
    pkgbase: &'a str,
    version: &'a str,
    platforms_str: &'a str,
    build_flags_str: &'a str,
    dep_names: &'a [String],
    dep_constraints: &'a HashMap<String, String>,
    pkgnames: &'a [String],
    source_type: SourceType,
    source_data_json: &'a str,
}

pub async fn package_add_with_client(
    client: &aurcache_deps::AurClient,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
    source_data: SourceData,
) -> anyhow::Result<String> {
    let platforms = match platforms {
        None => vec![Platform::X86_64],
        Some(platforms) => {
            check_platforms(&platforms)?;
            platforms
        }
    };

    let platforms_str = platforms
        .iter()
        .map(pacman_mirrors::platforms::Platform::as_str)
        .collect::<Vec<_>>()
        .join(";");

    let build_flags = build_flags.unwrap_or_else(|| {
        vec![
            "--noconfirm".to_string(),
            "--noprogressbar".to_string(),
            "--nocolor".to_string(),
            "--skippgpcheck".to_string(),
        ]
    });
    let build_flags_str = build_flags.join(";");

    match source_data {
        SourceData::Aur { ref name } => {
            add_aur_package(
                client,
                db,
                tx,
                &platforms,
                &platforms_str,
                &build_flags_str,
                name,
            )
            .await
        }
        SourceData::Git {
            ref r#ref,
            ref subfolder,
            ref url,
        } => {
            add_git_package(
                client,
                db,
                tx,
                &platforms,
                &platforms_str,
                &build_flags_str,
                r#ref,
                subfolder,
                url,
            )
            .await
        }
        SourceData::Upload { .. } => {
            todo!("upload")
        }
    }
}

pub async fn package_add(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
    source_data: SourceData,
) -> anyhow::Result<String> {
    let client = aurcache_deps::AurClient::new();
    package_add_with_client(&client, db, tx, platforms, build_flags, source_data).await
}

async fn set_directly_requested(db: &DatabaseConnection, pkgbase: &str) -> anyhow::Result<()> {
    packages::Entity::update_many()
        .col_expr(
            packages::Column::DirectlyRequested,
            sea_orm::sea_query::SimpleExpr::Value(sea_orm::Value::Bool(Some(true))),
        )
        .filter(packages::Column::Pkgbase.eq(pkgbase))
        .exec(db)
        .await?;
    Ok(())
}

async fn add_aur_package(
    client: &aurcache_deps::AurClient,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: &[Platform],
    platforms_str: &str,
    build_flags_str: &str,
    name: &str,
) -> anyhow::Result<String> {
    let pkg_name = name.trim();

    let bases = client
        .resolve_bases(&[pkg_name])
        .await
        .map_err(|e| anyhow!("AUR lookup failed: {e}"))?;
    let pkgbase = bases
        .get(pkg_name)
        .ok_or(anyhow!("Package '{pkg_name}' not found in AUR"))?
        .clone();

    if Packages::find()
        .filter(packages::Column::Pkgbase.eq(&pkgbase))
        .one(db)
        .await?
        .is_some()
    {
        set_directly_requested(db, &pkgbase).await?;
        return Ok(pkgbase);
    }

    let mut added_order: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    add_aur_package_recursive(
        client,
        db,
        &pkgbase,
        platforms_str,
        build_flags_str,
        &mut visited,
        &mut added_order,
    )
    .await?;

    set_directly_requested(db, &pkgbase).await?;

    trigger_leaf_builds(db, tx, platforms, &added_order).await?;

    Ok(pkgbase)
}

fn add_aur_package_recursive<'a>(
    client: &'a aurcache_deps::AurClient,
    db: &'a DatabaseConnection,
    pkgbase: &'a str,
    platforms_str: &'a str,
    build_flags_str: &'a str,
    visited: &'a mut HashSet<String>,
    added_order: &'a mut Vec<String>,
) -> AddRecursiveFut<'a> {
    Box::pin(async move {
        if !visited.insert(pkgbase.to_string()) {
            return Ok(());
        }

        if Packages::find()
            .filter(packages::Column::Pkgbase.eq(pkgbase))
            .one(db)
            .await?
            .is_some()
        {
            return Ok(());
        }

        let (version, deps) = client
            .deps_of_with_version(pkgbase)
            .await
            .map_err(|e| anyhow!("Failed to get deps for {pkgbase}: {e}"))?;

        let mut dep_constraints: HashMap<String, String> = HashMap::new();
        let dep_names: Vec<String> = deps
            .depends
            .iter()
            .chain(deps.make_depends.iter())
            .map(|d| {
                let (name, constraint) = crate::pkg::parse_dep(d);
                dep_constraints
                    .entry(name.to_string())
                    .or_insert(constraint.to_string());
                name.to_string()
            })
            .collect();

        let source_data_json = serde_json::to_string(&SourceData::Aur {
            name: pkgbase.to_string(),
        })?;

        let params = InsertPackageParams {
            pkgbase,
            version: &version,
            platforms_str,
            build_flags_str,
            dep_names: &dep_names,
            dep_constraints: &dep_constraints,
            pkgnames: &deps.pkgnames,
            source_type: SourceType::Aur,
            source_data_json: &source_data_json,
        };

        insert_package_with_deps(client, db, params, visited, added_order).await
    })
}

async fn insert_package_with_deps(
    client: &aurcache_deps::AurClient,
    db: &DatabaseConnection,
    params: InsertPackageParams<'_>,
    visited: &mut HashSet<String>,
    added_order: &mut Vec<String>,
) -> anyhow::Result<()> {
    let aur_dep_bases = if params.dep_names.is_empty() {
        HashMap::new()
    } else {
        let dep_refs: Vec<&str> = params.dep_names.iter().map(|s| s.as_str()).collect();
        client.resolve_bases(&dep_refs).await.map_err(|e| {
            anyhow!(
                "Failed to resolve AUR dependencies for {}: {e}",
                params.pkgbase
            )
        })?
    };

    let mut dep_pkgbases: Vec<String> = Vec::new();
    let mut dep_pkgbases_seen: HashSet<String> = HashSet::new();
    for dep_base in aur_dep_bases.values() {
        if dep_pkgbases_seen.insert(dep_base.clone()) {
            dep_pkgbases.push(dep_base.clone());
            add_aur_package_recursive(
                client,
                db,
                dep_base,
                params.platforms_str,
                params.build_flags_str,
                visited,
                added_order,
            )
            .await?;
        }
    }

    let split_packages_str = if params.pkgnames.len() > 1
        || params.pkgnames.first().is_none_or(|n| n != params.pkgbase)
    {
        Some(serde_json::to_string(params.pkgnames)?)
    } else {
        None
    };

    let new_package = packages::ActiveModel {
        name: Set(params.pkgbase.to_string()),
        pkgbase: Set(params.pkgbase.to_string()),
        status: Set(BuildStates::ENQUEUED_BUILD),
        upstream_version: Set(Some(params.version.to_string())),
        current_version: Set(Some(params.version.to_string())),
        platforms: Set(params.platforms_str.to_string()),
        build_flags: Set(params.build_flags_str.to_string()),
        source_type: Set(params.source_type),
        source_data: Set(params.source_data_json.to_string()),
        directly_requested: Set(false),
        split_packages: Set(split_packages_str),
        ..Default::default()
    };
    added_order.push(params.pkgbase.to_string());
    let txn = db.begin().await?;
    let saved = new_package.save(&txn).await?;

    let pkgbase_strs: Vec<&str> = dep_pkgbases.iter().map(|s| s.as_str()).collect();
    let dependees: HashMap<String, packages::Model> = Packages::find()
        .filter(packages::Column::Pkgbase.is_in(pkgbase_strs))
        .all(&txn)
        .await?
        .into_iter()
        .map(|p| (p.pkgbase.clone(), p))
        .collect();

    let base_to_name: HashMap<&str, &str> = aur_dep_bases
        .iter()
        .map(|(name, base)| (base.as_str(), name.as_str()))
        .collect();

    for dep_pkgbase in &dep_pkgbases {
        if let Some(dependee) = dependees.get(dep_pkgbase.as_str()) {
            let constraint = base_to_name
                .get(dep_pkgbase.as_str())
                .and_then(|name| params.dep_constraints.get(*name))
                .cloned()
                .unwrap_or_default();

            aurcache_db::dependencies::ActiveModel {
                dependent_id: Set(saved.id.clone().unwrap()),
                dependee_id: Set(dependee.id),
                platforms: Set(params.platforms_str.to_string()),
                version_constraint: Set(constraint),
                ..Default::default()
            }
            .save(&txn)
            .await?;
        }
    }

    txn.commit().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn add_git_package(
    client: &aurcache_deps::AurClient,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: &[Platform],
    platforms_str: &str,
    build_flags_str: &str,
    r#ref: &str,
    subfolder: &str,
    url: &str,
) -> anyhow::Result<String> {
    let dir = tempdir()?;
    let repo_path = dir.path().join("repo");

    checkout_repo_ref(url.to_string(), r#ref.to_string(), repo_path.clone())?;

    let sourceinfo =
        SourceInfoV1::from_pkgbuild(repo_path.join(subfolder).join("PKGBUILD").as_path())?;
    let version = sourceinfo.base.version.to_string();
    let pkgbase_name = sourceinfo.base.name.to_string();

    let srcinfo_deps = aurcache_deps::deps_from_srcinfo(&sourceinfo);
    let pkgnames = srcinfo_deps.pkgnames;
    let mut dep_constraints: HashMap<String, String> = HashMap::new();
    let mut dep_names: Vec<String> = Vec::new();
    let mut dep_set: HashSet<String> = HashSet::new();

    for d in srcinfo_deps
        .depends
        .iter()
        .chain(srcinfo_deps.make_depends.iter())
    {
        let (name, constraint) = crate::pkg::parse_dep(d);
        if dep_set.insert(name.to_string()) {
            dep_names.push(name.to_string());
        }
        dep_constraints
            .entry(name.to_string())
            .or_insert(constraint.to_string());
    }

    if Packages::find()
        .filter(packages::Column::Pkgbase.eq(&pkgbase_name))
        .one(db)
        .await?
        .is_some()
    {
        set_directly_requested(db, &pkgbase_name).await?;
        _ = dir.close();
        return Ok(pkgbase_name);
    }

    let source_data_json = serde_json::to_string(&SourceData::Git {
        url: url.to_string(),
        r#ref: r#ref.to_string(),
        subfolder: subfolder.to_string(),
    })?;

    let mut added_order: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    let params = InsertPackageParams {
        pkgbase: &pkgbase_name,
        version: &version,
        platforms_str,
        build_flags_str,
        dep_names: &dep_names,
        dep_constraints: &dep_constraints,
        pkgnames: &pkgnames,
        source_type: SourceType::Git,
        source_data_json: &source_data_json,
    };

    insert_package_with_deps(client, db, params, &mut visited, &mut added_order).await?;

    set_directly_requested(db, &pkgbase_name).await?;

    trigger_leaf_builds(db, tx, platforms, &added_order).await?;

    _ = dir.close();
    Ok(pkgbase_name)
}

async fn trigger_leaf_builds(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: &[Platform],
    pkgbases: &[String],
) -> anyhow::Result<()> {
    for pkgbase in pkgbases {
        let Some(pkg) = Packages::find()
            .filter(packages::Column::Pkgbase.eq(pkgbase))
            .one(db)
            .await?
        else {
            continue;
        };

        let dep_count = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(pkg.id))
            .count(db)
            .await?;

        if dep_count == 0 {
            let version = pkg
                .current_version
                .clone()
                .or(pkg.upstream_version.clone())
                .unwrap_or_default();
            trigger_build_for_package(db, tx, platforms, pkg, version).await?;
        }
    }
    Ok(())
}

async fn trigger_build_for_package(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: &[Platform],
    pkg: packages::Model,
    version: String,
) -> anyhow::Result<()> {
    for platform in platforms {
        let txn = db.begin().await?;

        let build = builds::ActiveModel {
            pkg_id: Set(pkg.id),
            output: Set(None),
            status: Set(Some(BuildStates::ENQUEUED_BUILD)),
            platform: Set(platform.to_string()),
            start_time: Set(Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Duration must exist")
                    .as_secs() as i64,
            )),
            version: Set(version.clone()),
            ..Default::default()
        };
        let new_build = build.save(&txn).await?;

        if *platform == Platform::X86_64 {
            let mut pkg_active: packages::ActiveModel = pkg.clone().into();
            pkg_active.latest_build = Set(Some(new_build.id.clone().unwrap()));
            pkg_active.save(&txn).await?;
        }

        txn.commit().await?;
        let _ = tx.send(Action::Build(
            Box::from(pkg.clone()),
            Box::from(new_build.try_into_model()?),
        ));
    }
    Ok(())
}

fn check_platforms(platforms: &Vec<Platform>) -> anyhow::Result<()> {
    for platform in platforms {
        if !Platforms.into_iter().any(|p| p == *platform) {
            bail!("Invalid platform: {platform}");
        }
    }
    Ok(())
}
