use crate::package::enqueue::trigger_initial_builds;
use crate::pkg::architectures_for_platforms;
use crate::snapshot::SnapshotStore;
use anyhow::{anyhow, bail};
use async_recursion::async_recursion;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::helpers::dependency_resolution::PackageCandidate;
use aurcache_db::packages;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::Packages;
use aurcache_deps::DependencyResolution;
use aurcache_types::builder::{Action, BuildStates};
use pacman_mirrors::platforms::{Platform, Platforms};
use sea_orm::QueryFilter;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait,
    TransactionTrait,
};
use std::collections::{HashMap, HashSet};
use tokio::sync::broadcast::Sender;

struct AddContext {
    platforms: Vec<Platform>,
    platforms_str: String,
    build_flags_str: String,
}

struct PackageInsertSpec {
    pkgbase: String,
    version: String,
    dep_names: Vec<String>,
    dep_constraints: HashMap<String, Option<crate::pkg::Constraint>>,
    pkgnames: Vec<String>,
    provides: Vec<String>,
    source_type: SourceType,
    source_data: SourceData,
}

/// A package an add intends to insert, with everything needed to write the row.
struct PlannedPackage {
    pkgbase: String,
    version: String,
    source_type: SourceType,
    source_data: SourceData,
    split_packages: Option<String>,
    provides: Option<String>,
    /// Only the package the user actually asked for; dependencies are not.
    directly_requested: bool,
}

/// A dependency edge, held by *name* because planned packages have no id until
/// the plan is persisted.
struct PlannedEdge {
    dependent: String,
    dependee: String,
    version_constraint: String,
}

/// Everything an add will write, resolved before anything is written.
///
/// Resolution walks the AUR and downloads repo databases, so doing it inside a
/// transaction would hold a write lock across the network — on SQLite that
/// blocks worker claims and build status updates for the duration. Planning
/// first keeps the transaction short *and* makes the add atomic: a failure
/// part-way through leaves nothing behind, where previously each package was
/// committed as it was resolved and a later failure left orphan rows that went
/// on to satisfy dependencies for subsequent adds.
#[derive(Default)]
struct AddPlan {
    /// Dependency-first, so inserting in order satisfies edges as they appear.
    packages: Vec<PlannedPackage>,
    edges: Vec<PlannedEdge>,
}

impl AddPlan {
    /// The planned packages as dependency-resolution candidates, so a package
    /// planned earlier in this add can satisfy a later one.
    fn candidates(&self) -> Vec<PackageCandidate> {
        self.packages
            .iter()
            .map(|pkg| PackageCandidate {
                name: pkg.pkgbase.clone(),
                split_packages: pkg.split_packages.clone(),
                provides: pkg.provides.clone(),
            })
            .collect()
    }
}

struct DependencyRequirements {
    dep_names: Vec<String>,
    dep_constraints: HashMap<String, Option<crate::pkg::Constraint>>,
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
) -> anyhow::Result<DependencyRequirements> {
    let mut dep_constraints: HashMap<String, Option<crate::pkg::Constraint>> = HashMap::new();
    let mut dep_names: Vec<String> = Vec::new();
    for dep in deps {
        let (name, constraint) = crate::pkg::parse_dep(dep);
        let constraint = crate::pkg::parse_dep_constraint(constraint);
        crate::pkg::merge_constraint_into(&mut dep_constraints, name, constraint)?;

        if !dep_names.iter().any(|seen| seen == name) {
            dep_names.push(name.to_string());
        }
    }
    Ok(DependencyRequirements {
        dep_names,
        dep_constraints,
    })
}

async fn package_exists(db: &DatabaseConnection, pkgbase: &str) -> anyhow::Result<bool> {
    Ok(Packages::find()
        .filter(packages::Column::Name.eq(pkgbase))
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

    // If the name resolves to a pkgbase via RPC (e.g. "czkawka-cli" → "czkawka")
    // use that; otherwise assume the input already *is* the pkgbase (e.g. "czkawka"
    // when it has no child package of the same name).
    Ok(bases
        .get(pkg_name)
        .cloned()
        .unwrap_or_else(|| pkg_name.to_string()))
}

async fn resolve_srcinfo_to_spec(
    store: &SnapshotStore,
    client: &aurcache_deps::AurClient,
    source_data: &SourceData,
    architectures: &[alpm_types::SystemArchitecture],
) -> anyhow::Result<PackageInsertSpec> {
    let sourceinfo = store.sourceinfo(client, source_data).await?;
    let deps = aurcache_deps::deps_from_srcinfo(&sourceinfo, architectures);
    let pkgbase = sourceinfo.base.name.to_string();
    let requirements =
        collect_dependency_requirements(deps.depends.iter().chain(deps.make_depends.iter()))?;

    Ok(PackageInsertSpec {
        pkgbase,
        version: sourceinfo.base.version.to_string(),
        dep_names: requirements.dep_names,
        dep_constraints: requirements.dep_constraints,
        pkgnames: deps.pkgnames,
        provides: deps.provides,
        source_type: match source_data {
            SourceData::Aur { .. } => SourceType::Aur,
            SourceData::Git { .. } => SourceType::Git,
            SourceData::Upload { .. } => SourceType::Upload,
        },
        source_data: source_data.clone(),
    })
}

async fn finalize_package_add(
    client: &aurcache_deps::AurClient,
    store: &SnapshotStore,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    context: &AddContext,
    package_spec: PackageInsertSpec,
) -> anyhow::Result<String> {
    if package_exists(db, &package_spec.pkgbase).await? {
        set_directly_requested(db, &package_spec.pkgbase).await?;
        return Ok(package_spec.pkgbase);
    }

    let mut visited: HashSet<String> = HashSet::from([package_spec.pkgbase.clone()]);
    let mut plan = AddPlan::default();
    let requested = package_spec.pkgbase.clone();

    plan_package_with_deps(
        client,
        store,
        db,
        package_spec,
        context,
        &mut visited,
        &mut plan,
    )
    .await?;

    // Only the package the user asked for is directly requested; everything
    // else in the plan is a dependency pulled in on its behalf.
    let Some(root) = plan
        .packages
        .iter_mut()
        .find(|pkg| pkg.pkgbase == requested)
    else {
        return Err(anyhow!("Package add produced no inserted packages"));
    };
    root.directly_requested = true;

    let added_order = persist_plan(db, context, plan).await?;
    let pkgbase = added_order
        .last()
        .cloned()
        .ok_or_else(|| anyhow!("Package add produced no inserted packages"))?;

    trigger_initial_builds(db, tx, &context.platforms, &added_order).await?;
    Ok(pkgbase)
}

pub async fn package_add_with_client(
    client: &aurcache_deps::AurClient,
    store: &SnapshotStore,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
    source_data: SourceData,
) -> anyhow::Result<String> {
    let context = build_add_context(platforms, build_flags)?;
    add_package_with_source(client, store, db, tx, &context, source_data).await
}

pub async fn package_add(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
    source_data: SourceData,
) -> anyhow::Result<String> {
    let client = aurcache_deps::AurClient::new();
    let store = SnapshotStore::new();
    package_add_with_client(&client, &store, db, tx, platforms, build_flags, source_data).await
}

async fn set_directly_requested(db: &DatabaseConnection, pkgbase: &str) -> anyhow::Result<()> {
    packages::Entity::update_many()
        .col_expr(
            packages::Column::DirectlyRequested,
            sea_orm::sea_query::SimpleExpr::Value(sea_orm::Value::Bool(Some(true))),
        )
        .filter(packages::Column::Name.eq(pkgbase))
        .exec(db)
        .await?;
    Ok(())
}

async fn add_package_with_source(
    client: &aurcache_deps::AurClient,
    store: &SnapshotStore,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    context: &AddContext,
    source_data: SourceData,
) -> anyhow::Result<String> {
    match &source_data {
        SourceData::Aur { name } => {
            let pkgbase = resolve_aur_pkgbase(client, name).await?;
            let aur_data = SourceData::Aur {
                name: pkgbase.clone(),
            };
            let package_spec = resolve_srcinfo_to_spec(
                store,
                client,
                &aur_data,
                &architectures_for_platforms(&context.platforms_str),
            )
            .await?;
            finalize_package_add(client, store, db, tx, context, package_spec).await
        }
        SourceData::Git { .. } => {
            let package_spec = resolve_srcinfo_to_spec(
                store,
                client,
                &source_data,
                &architectures_for_platforms(&context.platforms_str),
            )
            .await?;
            finalize_package_add(client, store, db, tx, context, package_spec).await
        }
        SourceData::Upload { .. } => {
            todo!("upload")
        }
    }
}

#[allow(clippy::double_must_use)]
#[async_recursion]
async fn plan_dependency_recursive(
    client: &aurcache_deps::AurClient,
    store: &SnapshotStore,
    db: &DatabaseConnection,
    pkgbase: &str,
    context: &AddContext,
    visited: &mut HashSet<String>,
    plan: &mut AddPlan,
) -> anyhow::Result<()> {
    // `visited` is the plan's name set: it already prevents planning the same
    // pkgbase twice, so the plan needs no separate "already planned?" lookup.
    if !visited.insert(pkgbase.to_string()) {
        return Ok(());
    }

    if package_exists(db, pkgbase).await? {
        return Ok(());
    }

    let source_data = SourceData::Aur {
        name: pkgbase.to_string(),
    };
    let package_spec = resolve_srcinfo_to_spec(
        store,
        client,
        &source_data,
        &architectures_for_platforms(&context.platforms_str),
    )
    .await?;
    plan_package_with_deps(client, store, db, package_spec, context, visited, plan).await
}

pub(crate) async fn ensure_aur_package_exists_recursive(
    client: &aurcache_deps::AurClient,
    store: &SnapshotStore,
    db: &DatabaseConnection,
    pkgbase: &str,
    platforms_str: &str,
    build_flags_str: &str,
) -> anyhow::Result<()> {
    // This helper inserts dependency-only rows and relies on the caller to
    // provide the platform/build flag strings that should be stored on them.
    let context = AddContext {
        platforms: vec![],
        platforms_str: platforms_str.to_string(),
        build_flags_str: build_flags_str.to_string(),
    };
    let mut visited = HashSet::new();
    let mut plan = AddPlan::default();
    plan_dependency_recursive(
        client,
        store,
        db,
        pkgbase,
        &context,
        &mut visited,
        &mut plan,
    )
    .await?;
    persist_plan(db, &context, plan).await?;
    Ok(())
}

pub(crate) async fn resolve_dependency_resolutions(
    client: &aurcache_deps::AurClient,
    db: &DatabaseConnection,
    dep_names: &[String],
) -> anyhow::Result<HashMap<String, DependencyResolution>> {
    aurcache_db::helpers::dependency_resolution::resolve_dependency_resolutions(
        client, db, dep_names,
    )
    .await
    .map_err(|e| anyhow!("Failed to resolve dependencies: {e}"))
}

/// Plan a package and, recursively, every AUR dependency it needs.
///
/// Writes nothing: results accumulate into `plan` so the whole graph can be
/// persisted in one transaction afterwards.
async fn plan_package_with_deps(
    client: &aurcache_deps::AurClient,
    store: &SnapshotStore,
    db: &DatabaseConnection,
    package_spec: PackageInsertSpec,
    context: &AddContext,
    visited: &mut HashSet<String>,
    plan: &mut AddPlan,
) -> anyhow::Result<()> {
    let resolved_deps = if package_spec.dep_names.is_empty() {
        HashMap::new()
    } else {
        // Packages planned earlier in this add are not in the database yet, so
        // they are offered as candidates alongside the rows that are.
        aurcache_db::helpers::dependency_resolution::resolve_dependency_resolutions_with_planned(
            client,
            db,
            &package_spec.dep_names,
            &plan.candidates(),
        )
        .await
        .map_err(|e| {
            anyhow!(
                "Failed to resolve dependencies for {}: {e}",
                package_spec.pkgbase
            )
        })?
    };

    // Iterate the declared dependency order rather than the resolution map's:
    // a HashMap's order varies per process, which would make the plan order —
    // and therefore the order builds are enqueued in — differ between runs for
    // identical input.
    let mut dep_pkgbases_seen: HashSet<String> = HashSet::new();
    for dep_name in &package_spec.dep_names {
        let Some(resolution) = resolved_deps.get(dep_name) else {
            continue;
        };
        let dep_base = match resolution {
            DependencyResolution::Official => continue,
            DependencyResolution::Local { pkgbase } | DependencyResolution::Aur { pkgbase } => {
                pkgbase
            }
        };
        if dep_base == &package_spec.pkgbase {
            continue;
        }
        if dep_pkgbases_seen.insert(dep_base.clone())
            && matches!(resolution, DependencyResolution::Aur { .. })
        {
            plan_dependency_recursive(client, store, db, dep_base, context, visited, plan).await?;
        }
    }

    let split_packages = split_packages_json(&package_spec.pkgbase, &package_spec.pkgnames)?;
    let provides = provides_json(&package_spec.provides)?;

    let mut dep_constraints_by_pkgbase: HashMap<String, Option<crate::pkg::Constraint>> =
        HashMap::new();
    for dep_name in &package_spec.dep_names {
        let Some(resolution) = resolved_deps.get(dep_name) else {
            continue;
        };
        let dep_pkgbase = match resolution {
            DependencyResolution::Official => continue,
            DependencyResolution::Local { pkgbase } | DependencyResolution::Aur { pkgbase } => {
                pkgbase
            }
        };
        if dep_pkgbase == &package_spec.pkgbase {
            continue;
        }
        let constraint = package_spec
            .dep_constraints
            .get(dep_name)
            .cloned()
            .flatten();

        crate::pkg::merge_constraint_into(
            &mut dep_constraints_by_pkgbase,
            dep_pkgbase,
            constraint,
        )?;
    }

    for (dep_pkgbase, constraint) in dep_constraints_by_pkgbase {
        plan.edges.push(PlannedEdge {
            dependent: package_spec.pkgbase.clone(),
            dependee: dep_pkgbase,
            version_constraint: constraint.map(|c| c.to_string()).unwrap_or_default(),
        });
    }

    // Pushed after its dependencies, keeping the plan dependency-first.
    plan.packages.push(PlannedPackage {
        pkgbase: package_spec.pkgbase,
        version: package_spec.version,
        source_type: package_spec.source_type,
        source_data: package_spec.source_data,
        split_packages,
        provides,
        directly_requested: false,
    });
    Ok(())
}

/// Write a planned add: every package and every edge, in one transaction.
///
/// Returns the inserted pkgbases in dependency-first order, for build
/// enqueueing.
async fn persist_plan(
    db: &DatabaseConnection,
    context: &AddContext,
    plan: AddPlan,
) -> anyhow::Result<Vec<String>> {
    let AddPlan { packages, edges } = plan;
    let txn = db.begin().await?;

    let mut ids: HashMap<String, i32> = HashMap::new();
    let mut added_order: Vec<String> = Vec::with_capacity(packages.len());
    for pkg in packages {
        let model = packages::ActiveModel {
            // `name` stores the pkgbase; this codebase keeps one row per package
            // base and tracks split package names separately.
            name: Set(pkg.pkgbase.clone()),
            status: Set(BuildStates::ENQUEUED_BUILD),
            upstream_version: Set(Some(pkg.version)),
            platforms: Set(context.platforms_str.clone()),
            build_flags: Set(context.build_flags_str.clone()),
            source_type: Set(pkg.source_type),
            source_data: Set(pkg.source_data),
            directly_requested: Set(pkg.directly_requested),
            split_packages: Set(pkg.split_packages),
            provides: Set(pkg.provides),
            ..Default::default()
        };
        let saved = model.save(&txn).await?;
        ids.insert(pkg.pkgbase.clone(), *saved.id.get()?);
        added_order.push(pkg.pkgbase);
    }

    // Edges may point at packages that already existed; resolve those once.
    let existing: Vec<&str> = edges
        .iter()
        .map(|edge| edge.dependee.as_str())
        .filter(|name| !ids.contains_key(*name))
        .collect();
    if !existing.is_empty() {
        for pkg in Packages::find()
            .filter(packages::Column::Name.is_in(existing))
            .all(&txn)
            .await?
        {
            ids.insert(pkg.name, pkg.id);
        }
    }

    for edge in edges {
        // A dependee that is neither planned nor already present has nothing to
        // link to; the dependent still builds against the official repos.
        let (Some(dependent_id), Some(dependee_id)) =
            (ids.get(&edge.dependent), ids.get(&edge.dependee))
        else {
            continue;
        };
        aurcache_db::dependencies::ActiveModel {
            dependent_id: Set(*dependent_id),
            dependee_id: Set(*dependee_id),
            version_constraint: Set(edge.version_constraint),
            ..Default::default()
        }
        .save(&txn)
        .await?;
    }

    txn.commit().await?;
    Ok(added_order)
}

pub(crate) fn split_packages_json(
    pkgbase: &str,
    pkgnames: &[String],
) -> anyhow::Result<Option<String>> {
    if pkgnames.len() <= 1 && pkgnames.first().is_none_or(|name| name == pkgbase) {
        return Ok(None);
    }

    Ok(Some(serde_json::to_string(pkgnames)?))
}

pub(crate) fn provides_json(provides: &[String]) -> anyhow::Result<Option<String>> {
    if provides.is_empty() {
        return Ok(None);
    }

    Ok(Some(serde_json::to_string(provides)?))
}

fn check_platforms(platforms: &Vec<Platform>) -> anyhow::Result<()> {
    for platform in platforms {
        if !Platforms.into_iter().any(|p| p == *platform) {
            bail!("Invalid platform: {platform}");
        }
    }
    Ok(())
}
