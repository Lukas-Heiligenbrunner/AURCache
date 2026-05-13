use crate::aur::api::get_package_info;
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

pub async fn package_add(
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
            add_aur_package(db, tx, &platforms, &platforms_str, &build_flags_str, name).await
        }
        SourceData::Git {
            ref r#ref,
            ref subfolder,
            ref url,
        } => {
            add_git_package(
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

async fn add_aur_package(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: &[Platform],
    platforms_str: &str,
    build_flags_str: &str,
    name: &str,
) -> anyhow::Result<String> {
    let client = aurcache_deps::AurClient::new();
    let pkg_name = name.trim();

    // Resolve base package name
    let bases = client
        .resolve_bases(&[pkg_name])
        .await
        .map_err(|e| anyhow!("AUR lookup failed: {e}"))?;
    let pkgbase = bases
        .get(pkg_name)
        .ok_or(anyhow!("Package '{pkg_name}' not found in AUR"))?
        .clone();

    // Check if base package already exists in DB
    if let Some(existing) = Packages::find()
        .filter(packages::Column::Pkgbase.eq(&pkgbase))
        .one(db)
        .await?
    {
        // Already exists - mark as directly requested
        let mut active: packages::ActiveModel = existing.into();
        active.directly_requested = Set(1);
        active.save(db).await?;
        return Ok(pkgbase);
    }

    // Collect all packages in dependency order (DFS, leaves first)
    let mut added_order: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    add_aur_package_recursive(
        &client,
        db,
        &pkgbase,
        platforms_str,
        build_flags_str,
        &mut visited,
        &mut added_order,
    )
    .await?;

    // The base package was already added by the recursive call.
    // Now mark it as directly requested.
    if let Some(pkg) = Packages::find()
        .filter(packages::Column::Pkgbase.eq(&pkgbase))
        .one(db)
        .await?
    {
        let mut active: packages::ActiveModel = pkg.into();
        active.directly_requested = Set(1);
        active.save(db).await?;
    }

    // Trigger builds for leaf packages only (those with no AUR dependencies).
    // Dependents will be triggered via cascade when their deps finish building.
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

        let deps = client
            .deps_of(pkgbase)
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

        let pkg_info = get_package_info(pkgbase)
            .await?
            .ok_or(anyhow!("Package {pkgbase} not found in AUR"))?;

        let source_data_json = serde_json::to_string(&SourceData::Aur {
            name: pkgbase.to_string(),
        })?;

        insert_package_with_deps(
            client,
            db,
            pkgbase,
            platforms_str,
            build_flags_str,
            dep_names,
            dep_constraints,
            deps.pkgnames,
            pkg_info.version,
            SourceType::Aur,
            source_data_json,
            visited,
            added_order,
        )
        .await
    })
}

async fn insert_package_with_deps(
    client: &aurcache_deps::AurClient,
    db: &DatabaseConnection,
    pkgbase: &str,
    platforms_str: &str,
    build_flags_str: &str,
    dep_names: Vec<String>,
    dep_constraints: HashMap<String, String>,
    pkgnames: Vec<String>,
    version: String,
    source_type: SourceType,
    source_data_json: String,
    visited: &mut HashSet<String>,
    added_order: &mut Vec<String>,
) -> anyhow::Result<()> {
    let aur_dep_bases = if dep_names.is_empty() {
        HashMap::new()
    } else {
        let dep_refs: Vec<&str> = dep_names.iter().map(|s| s.as_str()).collect();
        match client.resolve_bases(&dep_refs).await {
            Ok(bases) => bases,
            Err(e) => {
                tracing::warn!("Failed to resolve AUR deps for {pkgbase}: {e}");
                HashMap::new()
            }
        }
    };

    let mut dep_pkgbases: Vec<String> = Vec::new();
    for dep_base in aur_dep_bases.values() {
        if !dep_pkgbases.contains(dep_base) {
            dep_pkgbases.push(dep_base.clone());
            add_aur_package_recursive(
                client,
                db,
                dep_base,
                platforms_str,
                build_flags_str,
                visited,
                added_order,
            )
            .await?;
        }
    }

    let split_packages_str =
        if pkgnames.len() > 1 || pkgnames.first().map_or(true, |n| n != pkgbase) {
            Some(pkgnames.join(";"))
        } else {
            None
        };

    let new_package = packages::ActiveModel {
        name: Set(pkgbase.to_string()),
        pkgbase: Set(pkgbase.to_string()),
        status: Set(BuildStates::ENQUEUED_BUILD),
        upstream_version: Set(Some(version.clone())),
        current_version: Set(Some(version)),
        platforms: Set(platforms_str.to_string()),
        build_flags: Set(build_flags_str.to_string()),
        source_type: Set(source_type),
        source_data: Set(source_data_json),
        directly_requested: Set(0),
        split_packages: Set(split_packages_str),
        ..Default::default()
    };
    let saved = new_package.save(db).await?;
    added_order.push(pkgbase.to_string());

    for dep_pkgbase in &dep_pkgbases {
        if let Some(dependee) = Packages::find()
            .filter(packages::Column::Pkgbase.eq(dep_pkgbase))
            .one(db)
            .await?
        {
            let constraint = aur_dep_bases
                .iter()
                .find(|(_, v)| *v == dep_pkgbase)
                .and_then(|(name, _)| dep_constraints.get(name.as_str()))
                .cloned()
                .unwrap_or_default();

            aurcache_db::dependencies::ActiveModel {
                dependent_id: Set(saved.id.clone().unwrap()),
                dependee_id: Set(dependee.id),
                platforms: Set(platforms_str.to_string()),
                version_constraint: Set(constraint),
                ..Default::default()
            }
            .save(db)
            .await?;
        }
    }

    Ok(())
}

async fn add_git_package(
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

    use alpm_types::SystemArchitecture;
    let pkgnames: Vec<String> = sourceinfo
        .packages_for_architecture(SystemArchitecture::X86_64)
        .map(|p| p.name.to_string())
        .collect();

    // Extract dependency names and constraints from PKGBUILD
    let mut dep_constraints: HashMap<String, String> = HashMap::new();
    let mut dep_names: Vec<String> = Vec::new();
    for pkg in sourceinfo.packages_for_architecture(SystemArchitecture::X86_64) {
        for dep in &pkg.dependencies {
            let s = dep.to_string();
            let (name, constraint) = crate::pkg::parse_dep(&s);
            if !dep_names.contains(&name.to_string()) {
                dep_names.push(name.to_string());
            }
            dep_constraints
                .entry(name.to_string())
                .or_insert(constraint.to_string());
        }
        for dep in &pkg.make_dependencies {
            let s = dep.to_string();
            let (name, constraint) = crate::pkg::parse_dep(&s);
            if !dep_names.contains(&name.to_string()) {
                dep_names.push(name.to_string());
            }
            dep_constraints
                .entry(name.to_string())
                .or_insert(constraint.to_string());
        }
    }

    if let Some(existing) = Packages::find()
        .filter(packages::Column::Pkgbase.eq(&pkgbase_name))
        .one(db)
        .await?
    {
        let mut active: packages::ActiveModel = existing.into();
        active.directly_requested = Set(1);
        active.save(db).await?;
        _ = dir.close();
        return Ok(pkgbase_name);
    }

    let client = aurcache_deps::AurClient::new();
    let source_data_json = serde_json::to_string(&SourceData::Git {
        url: url.to_string(),
        r#ref: r#ref.to_string(),
        subfolder: subfolder.to_string(),
    })?;

    let mut added_order: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    insert_package_with_deps(
        &client,
        db,
        &pkgbase_name,
        platforms_str,
        build_flags_str,
        dep_names,
        dep_constraints,
        pkgnames,
        version.clone(),
        SourceType::Git,
        source_data_json,
        &mut visited,
        &mut added_order,
    )
    .await?;

    // Mark as directly requested
    if let Some(pkg) = Packages::find()
        .filter(packages::Column::Pkgbase.eq(&pkgbase_name))
        .one(db)
        .await?
    {
        let mut active: packages::ActiveModel = pkg.into();
        active.directly_requested = Set(1);
        active.save(db).await?;
    }

    // Trigger builds for leaf packages only
    trigger_leaf_builds(db, tx, platforms, &added_order).await?;

    _ = dir.close();
    Ok(pkgbase_name)
}

/// Trigger builds only for leaf packages (those with no AUR dependencies).
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
            Box::from(
                Packages::find_by_id(pkg.id)
                    .one(db)
                    .await?
                    .ok_or(anyhow!("Package not found"))?,
            ),
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
