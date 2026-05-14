use crate::git::checkout::checkout_repo_ref;
use crate::package::enqueue::trigger_leaf_builds;
use alpm_srcinfo::SourceInfoV1;
use anyhow::{anyhow, bail};
use aurcache_db::packages;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::Packages;
use aurcache_types::builder::{Action, BuildStates};
use pacman_mirrors::platforms::{Platform, Platforms};
use sea_orm::QueryFilter;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait,
    TransactionTrait,
};
use std::collections::{HashMap, HashSet};
use tempfile::tempdir;
use tokio::sync::broadcast::Sender;

type RecResult = std::result::Result<(), anyhow::Error>;
type AddRecursiveFut<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = RecResult> + Send + 'a>>;

struct AddContext {
    platforms: Vec<Platform>,
    platforms_str: String,
    build_flags_str: String,
}

struct DependencyRequirements {
    dep_names: Vec<String>,
    dep_constraints: HashMap<String, String>,
}

struct PackageInsertSpec {
    pkgbase: String,
    version: String,
    dep_names: Vec<String>,
    dep_constraints: HashMap<String, String>,
    pkgnames: Vec<String>,
    source_type: SourceType,
    source_data_json: String,
}

fn normalize_build_flags(flags: Vec<String>) -> Vec<String> {
    flags
        .into_iter()
        .map(|flag| flag.trim().to_string())
        .filter(|flag| !flag.is_empty())
        .collect()
}

fn build_add_context(
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
) -> anyhow::Result<AddContext> {
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

    let build_flags_str = normalize_build_flags(build_flags.unwrap_or_else(|| {
        vec![
            "--noconfirm".to_string(),
            "--noprogressbar".to_string(),
            "--nocolor".to_string(),
        ]
    }))
    .join(";");

    Ok(AddContext {
        platforms,
        platforms_str,
        build_flags_str,
    })
}

fn collect_dependency_requirements<'a>(
    deps: impl Iterator<Item = &'a String>,
) -> DependencyRequirements {
    let mut dep_constraints: HashMap<String, String> = HashMap::new();
    let mut dep_names: Vec<String> = Vec::new();

    for dep in deps {
        let (name, constraint) = crate::pkg::parse_dep(dep);
        dep_constraints
            .entry(name.to_string())
            .and_modify(|existing| {
                *existing = crate::pkg::merge_version_constraints(existing.as_str(), constraint);
            })
            .or_insert_with(|| constraint.to_string());

        if !dep_names.iter().any(|seen| seen == name) {
            dep_names.push(name.to_string());
        }
    }

    DependencyRequirements {
        dep_names,
        dep_constraints,
    }
}

async fn package_exists(db: &DatabaseConnection, pkgbase: &str) -> anyhow::Result<bool> {
    Ok(Packages::find()
        .filter(packages::Column::Pkgbase.eq(pkgbase))
        .one(db)
        .await?
        .is_some())
}

async fn resolve_aur_pkgbase(
    client: &aurcache_deps::AurClient,
    package_name: &str,
) -> anyhow::Result<String> {
    let pkg_name = package_name.trim();
    let bases = client
        .resolve_bases(&[pkg_name])
        .await
        .map_err(|e| anyhow!("AUR lookup failed: {e}"))?;

    bases
        .get(pkg_name)
        .cloned()
        .ok_or(anyhow!("Package '{pkg_name}' not found in AUR"))
}

async fn resolve_aur_package_spec(
    client: &aurcache_deps::AurClient,
    pkgbase: &str,
) -> anyhow::Result<PackageInsertSpec> {
    let (version, deps) = client
        .deps_of_with_version(pkgbase)
        .await
        .map_err(|e| anyhow!("Failed to get deps for {pkgbase}: {e}"))?;
    let requirements =
        collect_dependency_requirements(deps.depends.iter().chain(deps.make_depends.iter()));

    Ok(PackageInsertSpec {
        pkgbase: pkgbase.to_string(),
        version,
        dep_names: requirements.dep_names,
        dep_constraints: requirements.dep_constraints,
        pkgnames: deps.pkgnames,
        source_type: SourceType::Aur,
        source_data_json: serde_json::to_string(&SourceData::Aur {
            name: pkgbase.to_string(),
        })?,
    })
}

fn resolve_git_package_spec(
    sourceinfo: &SourceInfoV1,
    url: &str,
    r#ref: &str,
    subfolder: &str,
) -> anyhow::Result<PackageInsertSpec> {
    let deps = aurcache_deps::deps_from_srcinfo(sourceinfo);
    let requirements =
        collect_dependency_requirements(deps.depends.iter().chain(deps.make_depends.iter()));

    Ok(PackageInsertSpec {
        pkgbase: sourceinfo.base.name.to_string(),
        version: sourceinfo.base.version.to_string(),
        dep_names: requirements.dep_names,
        dep_constraints: requirements.dep_constraints,
        pkgnames: deps.pkgnames,
        source_type: SourceType::Git,
        source_data_json: serde_json::to_string(&SourceData::Git {
            url: url.to_string(),
            r#ref: r#ref.to_string(),
            subfolder: subfolder.to_string(),
        })?,
    })
}

async fn finalize_package_add(
    client: &aurcache_deps::AurClient,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    context: &AddContext,
    package_spec: PackageInsertSpec,
) -> anyhow::Result<String> {
    if package_exists(db, &package_spec.pkgbase).await? {
        set_directly_requested(db, &package_spec.pkgbase).await?;
        return Ok(package_spec.pkgbase);
    }

    let mut added_order: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::from([package_spec.pkgbase.clone()]);

    insert_package_with_deps(
        client,
        db,
        package_spec,
        context,
        &mut visited,
        &mut added_order,
    )
    .await?;

    let pkgbase = added_order
        .last()
        .cloned()
        .ok_or_else(|| anyhow!("Package add produced no inserted packages"))?;

    set_directly_requested(db, &pkgbase).await?;
    trigger_leaf_builds(db, tx, &context.platforms, &added_order).await?;
    Ok(pkgbase)
}

pub async fn package_add_with_client(
    client: &aurcache_deps::AurClient,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
    source_data: SourceData,
) -> anyhow::Result<String> {
    let context = build_add_context(platforms, build_flags)?;

    match source_data {
        SourceData::Aur { ref name } => add_aur_package(client, db, tx, &context, name).await,
        SourceData::Git {
            ref r#ref,
            ref subfolder,
            ref url,
        } => add_git_package(client, db, tx, &context, r#ref, subfolder, url).await,
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
    context: &AddContext,
    name: &str,
) -> anyhow::Result<String> {
    let pkgbase = resolve_aur_pkgbase(client, name).await?;
    let package_spec = resolve_aur_package_spec(client, &pkgbase).await?;
    finalize_package_add(client, db, tx, context, package_spec).await
}

fn add_aur_package_recursive<'a>(
    client: &'a aurcache_deps::AurClient,
    db: &'a DatabaseConnection,
    pkgbase: &'a str,
    context: &'a AddContext,
    visited: &'a mut HashSet<String>,
    added_order: &'a mut Vec<String>,
) -> AddRecursiveFut<'a> {
    Box::pin(async move {
        if !visited.insert(pkgbase.to_string()) {
            return Ok(());
        }

        if package_exists(db, pkgbase).await? {
            return Ok(());
        }

        let package_spec = resolve_aur_package_spec(client, pkgbase).await?;
        insert_package_with_deps(client, db, package_spec, context, visited, added_order).await
    })
}

pub(crate) async fn ensure_aur_package_exists_recursive(
    client: &aurcache_deps::AurClient,
    db: &DatabaseConnection,
    pkgbase: &str,
    platforms_str: &str,
    build_flags_str: &str,
) -> anyhow::Result<()> {
    let context = AddContext {
        platforms: vec![],
        platforms_str: platforms_str.to_string(),
        build_flags_str: build_flags_str.to_string(),
    };
    let mut visited = HashSet::new();
    let mut added_order = Vec::new();
    add_aur_package_recursive(
        client,
        db,
        pkgbase,
        &context,
        &mut visited,
        &mut added_order,
    )
    .await
}

async fn insert_package_with_deps(
    client: &aurcache_deps::AurClient,
    db: &DatabaseConnection,
    package_spec: PackageInsertSpec,
    context: &AddContext,
    visited: &mut HashSet<String>,
    added_order: &mut Vec<String>,
) -> anyhow::Result<()> {
    let aur_dep_bases = if package_spec.dep_names.is_empty() {
        HashMap::new()
    } else {
        let dep_refs: Vec<&str> = package_spec.dep_names.iter().map(|s| s.as_str()).collect();
        client.resolve_bases(&dep_refs).await.map_err(|e| {
            anyhow!(
                "Failed to resolve AUR dependencies for {}: {e}",
                package_spec.pkgbase
            )
        })?
    };

    let mut dep_pkgbases: Vec<String> = Vec::new();
    let mut dep_pkgbases_seen: HashSet<String> = HashSet::new();
    for dep_base in aur_dep_bases.values() {
        if dep_pkgbases_seen.insert(dep_base.clone()) {
            dep_pkgbases.push(dep_base.clone());
            add_aur_package_recursive(client, db, dep_base, context, visited, added_order).await?;
        }
    }

    let split_packages_str = if package_spec.pkgnames.len() > 1
        || package_spec
            .pkgnames
            .first()
            .is_none_or(|n| n != &package_spec.pkgbase)
    {
        Some(serde_json::to_string(&package_spec.pkgnames)?)
    } else {
        None
    };

    let new_package = packages::ActiveModel {
        name: Set(package_spec.pkgbase.clone()),
        pkgbase: Set(package_spec.pkgbase.clone()),
        status: Set(BuildStates::ENQUEUED_BUILD),
        upstream_version: Set(Some(package_spec.version.clone())),
        current_version: Set(Some(package_spec.version.clone())),
        platforms: Set(context.platforms_str.clone()),
        build_flags: Set(context.build_flags_str.clone()),
        source_type: Set(package_spec.source_type),
        source_data: Set(package_spec.source_data_json),
        directly_requested: Set(false),
        split_packages: Set(split_packages_str),
        ..Default::default()
    };
    added_order.push(package_spec.pkgbase.clone());
    let txn = db.begin().await?;
    let saved = new_package.save(&txn).await?;

    let mut dep_constraints_by_pkgbase: HashMap<String, String> = HashMap::new();
    for dep_name in &package_spec.dep_names {
        let Some(dep_pkgbase) = aur_dep_bases.get(dep_name) else {
            continue;
        };
        let constraint = package_spec
            .dep_constraints
            .get(dep_name)
            .map_or("", String::as_str);

        dep_constraints_by_pkgbase
            .entry(dep_pkgbase.clone())
            .and_modify(|existing| {
                *existing = crate::pkg::merge_version_constraints(existing.as_str(), constraint);
            })
            .or_insert_with(|| constraint.to_string());
    }

    let pkgbase_strs: Vec<&str> = dep_pkgbases.iter().map(|s| s.as_str()).collect();
    let dependees: HashMap<String, packages::Model> = Packages::find()
        .filter(packages::Column::Pkgbase.is_in(pkgbase_strs))
        .all(&txn)
        .await?
        .into_iter()
        .map(|p| (p.pkgbase.clone(), p))
        .collect();

    for dep_pkgbase in &dep_pkgbases {
        if let Some(dependee) = dependees.get(dep_pkgbase.as_str()) {
            let constraint = dep_constraints_by_pkgbase
                .get(dep_pkgbase.as_str())
                .cloned()
                .unwrap_or_default();

            aurcache_db::dependencies::ActiveModel {
                dependent_id: Set(saved.id.clone().unwrap()),
                dependee_id: Set(dependee.id),
                platforms: Set(context.platforms_str.clone()),
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
    context: &AddContext,
    r#ref: &str,
    subfolder: &str,
    url: &str,
) -> anyhow::Result<String> {
    let dir = tempdir()?;
    let repo_path = dir.path().join("repo");

    checkout_repo_ref(url.to_string(), r#ref.to_string(), repo_path.clone())?;

    let sourceinfo =
        SourceInfoV1::from_pkgbuild(repo_path.join(subfolder).join("PKGBUILD").as_path())?;
    let package_spec = resolve_git_package_spec(&sourceinfo, url, r#ref, subfolder)?;
    let result = finalize_package_add(client, db, tx, context, package_spec).await;
    _ = dir.close();
    result
}

fn check_platforms(platforms: &Vec<Platform>) -> anyhow::Result<()> {
    for platform in platforms {
        if !Platforms.into_iter().any(|p| p == *platform) {
            bail!("Invalid platform: {platform}");
        }
    }
    Ok(())
}
