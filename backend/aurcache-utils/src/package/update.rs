use crate::package::add::{
    ensure_aur_package_exists_recursive, provides_json, resolve_dependency_resolutions,
    split_packages_json,
};
use crate::snapshot::SnapshotStore;
use alpm_types::Version;
use anyhow::{anyhow, bail};
use async_recursion::async_recursion;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::package_update_activity::PackageUpdateActivity;
use aurcache_db::activities::ActivityType;
use aurcache_db::helpers::build_enqueue::{enqueue_build_if_missing, promote_waiting_build};
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use aurcache_db::{builds, dependencies, packages};
use aurcache_deps::{AurClient, DependencyResolution, PkgDeps};
use aurcache_types::builder::{Action, BuildStates};
use pacman_mirrors::platforms::Platform;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait,
};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;
use tracing::info;

/// Remove packages that have no remaining dependents and are not directly requested.
async fn remove_orphaned_packages(db: &DatabaseConnection, exclude_id: i32) -> anyhow::Result<()> {
    let candidates = Packages::find()
        .filter(packages::Column::DirectlyRequested.eq(false))
        .filter(packages::Column::Id.ne(exclude_id))
        .all(db)
        .await?;

    for pkg in &candidates {
        let dep_count = Dependencies::find()
            .filter(dependencies::Column::DependeeId.eq(pkg.id))
            .count(db)
            .await?;
        if dep_count > 0 {
            continue;
        }
        let txn = db.begin().await?;
        builds::Entity::delete_many()
            .filter(builds::Column::PkgId.eq(pkg.id))
            .exec(&txn)
            .await?;
        dependencies::Entity::delete_many()
            .filter(dependencies::Column::DependentId.eq(pkg.id))
            .exec(&txn)
            .await?;
        packages::Entity::delete_by_id(pkg.id).exec(&txn).await?;
        txn.commit().await?;
    }
    Ok(())
}

/// Update every package currently marked as outdated.
///
/// Only packages whose latest build completed successfully are retriggered.
/// Returns the build IDs enqueued across all updated packages.
pub async fn package_update_all_outdated(
    db: &DatabaseConnection,
    store: &SnapshotStore,
    tx: &Sender<Action>,
) -> anyhow::Result<Vec<i32>> {
    let pkg_models: Vec<packages::Model> = Packages::find()
        .filter(packages::Column::OutOfDate.eq(1))
        .all(db)
        .await?;
    let activity_log = ActivityLog::new(db.clone());
    let client = AurClient::new();

    let mut ids_total = vec![];
    for pkg in &pkg_models {
        if pkg.status == BuildStates::SUCCESSFUL_BUILD {
            let results =
                package_update_with_client(&client, store, db, pkg.to_owned(), false, tx).await?;
            activity_log
                .add(
                    PackageUpdateActivity {
                        package: pkg.name.clone(),
                        forced: false,
                    },
                    ActivityType::UpdatePackage,
                    Some("Server".to_string()),
                )
                .await?;
            ids_total.extend(
                results
                    .into_iter()
                    .filter(|r| r.enqueued)
                    .map(|r| r.build_id),
            );
        } else {
            info!(
                "Package auto update was not triggered for package {} because of prev. build status: {}",
                pkg.name, pkg.status
            );
        }
    }
    Ok(ids_total)
}

/// Updates a single package for all required platforms.
///
/// This function fetches the latest package metadata and updates it if necessary.
///
/// # Arguments
///
/// * `db` - A reference to the database connection.
/// * `pkg_model` - The package model to update.
/// * `force` - A boolean flag to force an update even if the package version is unchanged.
/// * `tx` - A broadcast channel sender for triggering build actions.
///
/// # Returns
///
/// * `Ok(Vec<PlatformUpdateResult>)` - One entry per configured platform, describing the build
///   that was enqueued/promoted or left waiting on dependencies.
/// * `Err(anyhow::Error)` - If any error occurs during the update trigger.
pub async fn package_update(
    db: &DatabaseConnection,
    pkg_model: packages::Model,
    force: bool,
    tx: &Sender<Action>,
) -> anyhow::Result<Vec<PlatformUpdateResult>> {
    let client = AurClient::new();
    let store = SnapshotStore::new();
    package_update_with_client(&client, &store, db, pkg_model, force, tx).await
}

/// Update a single package using a caller-provided AUR client.
///
/// Returns one [`PlatformUpdateResult`] per configured platform.
pub async fn package_update_with_client(
    client: &AurClient,
    store: &SnapshotStore,
    db: &DatabaseConnection,
    pkg_model: packages::Model,
    force: bool,
    tx: &Sender<Action>,
) -> anyhow::Result<Vec<PlatformUpdateResult>> {
    let mut visited = HashSet::new();
    let mut services = Services {
        client,
        store,
        db,
        tx,
    };
    package_update_with_client_inner(&mut services, pkg_model, force, &mut visited).await
}

/// Recursively update a package and its dependencies, enqueuing builds for ready platforms.
#[allow(clippy::double_must_use)]
#[async_recursion]
async fn package_update_with_client_inner(
    services: &mut Services<'_>,
    pkg_model: packages::Model,
    force: bool,
    visited: &mut HashSet<i32>,
) -> anyhow::Result<Vec<PlatformUpdateResult>> {
    if !visited.insert(pkg_model.id) {
        return Ok(vec![]);
    }

    let sourceinfo = services
        .store
        .sourceinfo(services.client, &pkg_model.source_data)
        .await
        .map_err(|e| anyhow!("Failed to resolve source info: {e}"))?;
    let upstream_version = sourceinfo.base.version.to_string();
    let deps = aurcache_deps::deps_from_srcinfo(&sourceinfo);

    let graph = sync_dependency_graph(services, &pkg_model, &deps).await?;

    // With the update, it's possible some dependencies are no longer needed.
    remove_orphaned_packages(services.db, pkg_model.id).await?;

    let latest_build = Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_model.id))
        .order_by_desc(builds::Column::StartTime)
        .one(services.db)
        .await?;

    if let Some(build) = latest_build
        && !force
        && build.version == upstream_version
    {
        bail!(
            "Latest build is already up to date (version {})",
            upstream_version
        );
    }

    let platform_results = enqueue_platform_builds(
        services,
        BuildRequest {
            pkg_model: &pkg_model,
            version: &upstream_version,
            graph: &graph,
        },
        visited,
    )
    .await?;

    let any_enqueued = platform_results.iter().any(|r| r.enqueued);
    let has_waiting = platform_results.iter().any(|r| !r.enqueued);

    let pkgbase = sourceinfo.base.name.to_string();
    let mut pkg_model_active: packages::ActiveModel = pkg_model.clone().into();
    let initial_status = if has_waiting && !any_enqueued {
        BuildStates::WAITING_FOR_DEPS
    } else {
        BuildStates::ENQUEUED_BUILD
    };
    pkg_model_active.status = Set(initial_status);
    pkg_model_active.upstream_version = Set(Some(upstream_version.clone()));
    pkg_model_active.split_packages = Set(split_packages_json(&pkgbase, &deps.pkgnames)?);
    pkg_model_active.provides = Set(provides_json(&deps.provides)?);
    let txn = services.db.begin().await?;
    pkg_model_active.save(&txn).await?;
    txn.commit().await?;

    Ok(platform_results)
}

/// A single dependency of the package being updated, with its constraint.
struct DepInfo {
    constraint: Option<crate::pkg::Constraint>,
    package: packages::Model,
}

/// Dependencies resolved for the current update, keyed by pkgbase.
struct DependencyGraph {
    deps: HashMap<String, DepInfo>,
}

/// Resolve dependency constraints, ensure all dependees exist in the DB,
/// and sync the dependency rows.
///
/// Does not care about builds at this point.
async fn sync_dependency_graph(
    services: &mut Services<'_>,
    pkg_model: &packages::Model,
    deps: &PkgDeps,
) -> anyhow::Result<DependencyGraph> {
    let dep_constraints_by_pkgbase =
        resolve_dependency_constraints(services, pkg_model, deps).await?;

    ensure_missing_dependency_packages(services, pkg_model, &dep_constraints_by_pkgbase).await?;

    if dep_constraints_by_pkgbase.is_empty() {
        sync_dependency_rows(
            services.db,
            pkg_model.id,
            &dep_constraints_by_pkgbase,
            &HashMap::new(),
        )
        .await?;
        return Ok(DependencyGraph {
            deps: HashMap::new(),
        });
    }

    let dep_packages = fetch_dep_packages_map(services.db, &dep_constraints_by_pkgbase).await?;

    sync_dependency_rows(
        services.db,
        pkg_model.id,
        &dep_constraints_by_pkgbase,
        &dep_packages,
    )
    .await?;

    let deps_map = dep_constraints_by_pkgbase
        .into_iter()
        .map(|(pkgbase, constraint)| {
            let package = dep_packages
                .get(&pkgbase)
                .cloned()
                .expect("dep package must exist after ensure");
            (
                pkgbase,
                DepInfo {
                    constraint,
                    package,
                },
            )
        })
        .collect();

    Ok(DependencyGraph { deps: deps_map })
}

/// Collect and resolve dependency constraints to pkgbase names.
async fn resolve_dependency_constraints(
    services: &mut Services<'_>,
    pkg_model: &packages::Model,
    deps: &PkgDeps,
) -> anyhow::Result<HashMap<String, Option<crate::pkg::Constraint>>> {
    let dep_constraints = collect_dependency_constraints(deps)?;
    resolve_dependency_constraints_by_pkgbase(
        services.client,
        services.db,
        &pkg_model.name,
        &dep_constraints,
    )
    .await
}

/// Ensure all resolved dependency packages exist in the database,
/// adding them via the AUR if missing.
async fn ensure_missing_dependency_packages(
    services: &mut Services<'_>,
    pkg_model: &packages::Model,
    dep_constraints_by_pkgbase: &HashMap<String, Option<crate::pkg::Constraint>>,
) -> anyhow::Result<()> {
    for dep_pkgbase in dep_constraints_by_pkgbase.keys() {
        if Packages::find()
            .filter(packages::Column::Name.eq(dep_pkgbase.as_str()))
            .one(services.db)
            .await?
            .is_none()
        {
            ensure_aur_package_exists_recursive(
                services.client,
                services.store,
                services.db,
                dep_pkgbase,
                &pkg_model.platforms,
                &pkg_model.build_flags,
            )
            .await?;
        }
    }
    Ok(())
}

/// Fetch a name→model map of all dependency packages from the database.
async fn fetch_dep_packages_map(
    db: &DatabaseConnection,
    dep_constraints_by_pkgbase: &HashMap<String, Option<crate::pkg::Constraint>>,
) -> anyhow::Result<HashMap<String, packages::Model>> {
    Ok(Packages::find()
        .filter(
            packages::Column::Name.is_in(
                dep_constraints_by_pkgbase
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
        )
        .all(db)
        .await?
        .into_iter()
        .map(|pkg| (pkg.name.clone(), pkg))
        .collect())
}

/// Merge depends and make_depends into a single map of package name → optional version constraint.
fn collect_dependency_constraints(
    deps: &PkgDeps,
) -> anyhow::Result<HashMap<String, Option<crate::pkg::Constraint>>> {
    let mut dep_constraints: HashMap<String, Option<crate::pkg::Constraint>> = HashMap::new();
    for dep in deps.depends.iter().chain(deps.make_depends.iter()) {
        let (name, constraint) = aurcache_deps::parse_dep(dep);
        let constraint = crate::pkg::parse_dep_constraint(constraint);
        crate::pkg::merge_constraint_into(&mut dep_constraints, name, constraint)?;
    }

    Ok(dep_constraints)
}

/// Resolve dependency names to their pkgbase and merge constraints keyed by pkgbase.
async fn resolve_dependency_constraints_by_pkgbase(
    client: &AurClient,
    db: &DatabaseConnection,
    pkgbase: &str,
    dep_constraints: &HashMap<String, Option<crate::pkg::Constraint>>,
) -> anyhow::Result<HashMap<String, Option<crate::pkg::Constraint>>> {
    if dep_constraints.is_empty() {
        return Ok(HashMap::new());
    }

    let dep_names = dep_constraints.keys().cloned().collect::<Vec<_>>();
    let resolved_deps = resolve_dependency_resolutions(client, db, &dep_names).await?;

    let mut dep_constraints_by_pkgbase: HashMap<String, Option<crate::pkg::Constraint>> =
        HashMap::new();
    for (dep_name, resolution) in resolved_deps {
        let dep_pkgbase = match resolution {
            DependencyResolution::Official => continue,
            DependencyResolution::Local { pkgbase } | DependencyResolution::Aur { pkgbase } => {
                pkgbase
            }
        };
        if dep_pkgbase == pkgbase {
            continue;
        }
        let constraint = dep_constraints.get(dep_name.as_str()).cloned().flatten();

        crate::pkg::merge_constraint_into(
            &mut dep_constraints_by_pkgbase,
            &dep_pkgbase,
            constraint,
        )?;
    }

    Ok(dep_constraints_by_pkgbase)
}

/// Insert, update, or remove dependency rows to match the current constraint set.
async fn sync_dependency_rows(
    db: &DatabaseConnection,
    dependent_id: i32,
    dep_constraints_by_pkgbase: &HashMap<String, Option<crate::pkg::Constraint>>,
    dep_packages: &HashMap<String, packages::Model>,
) -> anyhow::Result<()> {
    let txn = db.begin().await?;
    let desired_dependee_ids = dep_packages.values().map(|pkg| pkg.id).collect::<Vec<_>>();

    for existing in Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent_id))
        .all(&txn)
        .await?
    {
        if !desired_dependee_ids.contains(&existing.dependee_id) {
            existing.delete(&txn).await?;
        }
    }

    for (dep_pkgbase, constraint) in dep_constraints_by_pkgbase {
        let Some(dep_pkg) = dep_packages.get(dep_pkgbase) else {
            continue;
        };
        let serialized = constraint
            .as_ref()
            .map(|c| c.to_string())
            .unwrap_or_default();

        if let Some(existing) = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(dependent_id))
            .filter(dependencies::Column::DependeeId.eq(dep_pkg.id))
            .one(&txn)
            .await?
        {
            let mut active: dependencies::ActiveModel = existing.into();
            active.version_constraint = Set(serialized.clone());
            active.save(&txn).await?;
        } else {
            dependencies::ActiveModel {
                dependent_id: Set(dependent_id),
                dependee_id: Set(dep_pkg.id),
                version_constraint: Set(serialized),
                ..Default::default()
            }
            .save(&txn)
            .await?;
        }
    }

    txn.commit().await?;
    Ok(())
}

/// Check whether a successful build of `dependee` satisfies the given version constraint.
async fn dependency_satisfies_constraint(
    db: &DatabaseConnection,
    dependee_id: i32,
    platform: &Platform,
    constraint: Option<&crate::pkg::Constraint>,
) -> anyhow::Result<bool> {
    let Some(build) = Builds::find()
        .select_only()
        .column(builds::Column::Version)
        .filter(builds::Column::PkgId.eq(dependee_id))
        .filter(builds::Column::Platform.eq(platform.as_str()))
        .filter(builds::Column::Status.eq(Some(BuildStates::SUCCESSFUL_BUILD)))
        .order_by(builds::Column::EndTime, sea_orm::Order::Desc)
        .order_by(builds::Column::StartTime, sea_orm::Order::Desc)
        .into_tuple::<(String,)>()
        .one(db)
        .await?
    else {
        return Ok(false);
    };

    let Some(constraint) = constraint else {
        return Ok(true);
    };
    let Ok(version) = Version::from_str(&build.0) else {
        return Ok(false);
    };
    Ok(constraint.is_satisfied(&version))
}

/// Check whether the dependencies for a package are satisfied on a single platform.
///
/// If a dependency needs a rebuild and no build is pending, this triggers the
/// recursive update so the dependency will be available when the dependent starts.
/// Packages whose last build failed are never auto-retriggered — the user must
/// Check whether all deps in the graph are satisfied or have pending builds on this platform.
async fn dependencies_ready_for_platform(
    services: &mut Services<'_>,
    platform: &Platform,
    graph: &DependencyGraph,
    visited: &mut HashSet<i32>,
) -> anyhow::Result<bool> {
    for dep_info in graph.deps.values() {
        if dependency_satisfies_constraint(
            services.db,
            dep_info.package.id,
            platform,
            dep_info.constraint.as_ref(),
        )
        .await?
        {
            continue;
        }

        let has_pending_build = Builds::find()
            .filter(builds::Column::PkgId.eq(dep_info.package.id))
            .filter(builds::Column::Platform.eq(platform.as_str()))
            .filter(builds::Column::Status.is_in(vec![
                Some(BuildStates::ENQUEUED_BUILD),
                Some(BuildStates::ACTIVE_BUILD),
                Some(BuildStates::WAITING_FOR_DEPS),
            ]))
            .count(services.db)
            .await?
            > 0;

        if !has_pending_build {
            if dep_info.package.status == BuildStates::FAILED_BUILD {
                // Last build failed — don't auto-retry.
            } else {
                package_update_with_client_inner(services, dep_info.package.clone(), true, visited)
                    .await?;
            }
        }

        return Ok(false);
    }

    Ok(true)
}

/// Shared service dependencies passed through the update pipeline.
struct Services<'a> {
    client: &'a AurClient,
    store: &'a SnapshotStore,
    db: &'a DatabaseConnection,
    tx: &'a Sender<Action>,
}

/// What to build and its resolved dependency graph.
struct BuildRequest<'a> {
    pkg_model: &'a packages::Model,
    version: &'a str,
    graph: &'a DependencyGraph,
}

/// Outcome of triggering an update for a single platform of a package.
#[derive(Debug, Clone)]
pub struct PlatformUpdateResult {
    pub platform: Platform,
    /// The build row created/reused for this platform. Present regardless of
    /// whether the build was actually dispatched or is still waiting on a
    /// dependency rebuild.
    pub build_id: i32,
    /// `true` if the build was enqueued/promoted and dispatched to the builder;
    /// `false` if it was left `WAITING_FOR_DEPS` pending an unfinished
    /// dependency rebuild.
    pub enqueued: bool,
}

/// For each configured platform, check dep readiness and enqueue builds.
async fn enqueue_platform_builds(
    services: &mut Services<'_>,
    request: BuildRequest<'_>,
    visited: &mut HashSet<i32>,
) -> anyhow::Result<Vec<PlatformUpdateResult>> {
    let configured_platforms =
        Platform::parse_many(&request.pkg_model.platforms).collect::<Result<Vec<_>, _>>()?;

    let mut results = Vec::new();

    for platform in &configured_platforms {
        let ready =
            dependencies_ready_for_platform(services, platform, request.graph, visited).await?;

        if ready {
            let result = update_platform(
                *platform,
                request.pkg_model.clone(),
                request.version.to_string(),
                services.db,
                services.tx,
            )
            .await?;
            results.push(PlatformUpdateResult {
                platform: *platform,
                build_id: result.build.id,
                enqueued: result.inserted,
            });
        } else {
            let txn = services.db.begin().await?;
            let start_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
            let waiting = enqueue_build_if_missing(
                &txn,
                request.pkg_model.id,
                *platform,
                request.version,
                start_time,
                BuildStates::WAITING_FOR_DEPS,
            )
            .await?;
            txn.commit().await?;
            results.push(PlatformUpdateResult {
                platform: *platform,
                build_id: waiting.build.id,
                enqueued: false,
            });
        }
    }

    Ok(results)
}

/// Create or reuse the pending build entry for a package on one platform.
///
/// If a `WAITING_FOR_DEPS` build already exists for this `(pkg, platform)`, it is promoted to
/// `ENQUEUED` and dispatched rather than inserting a duplicate.  This happens when a dependency
/// finishes and the dependent was already in the pending queue waiting for it.
pub async fn update_platform(
    platform: Platform,
    pkg: packages::Model,
    new_version: String,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
) -> anyhow::Result<aurcache_db::helpers::build_enqueue::EnqueueBuildResult> {
    // Fast path: promote an existing WAITING_FOR_DEPS build if one is present.
    if let Some(promoted) = promote_waiting_build(db, pkg.id, platform).await? {
        let _ = tx.send(Action::Build(Box::from(pkg), Box::from(promoted.clone())));
        return Ok(aurcache_db::helpers::build_enqueue::EnqueueBuildResult {
            build: promoted,
            inserted: true,
        });
    }

    let txn = db.begin().await?;
    let start_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let enqueue_result = enqueue_build_if_missing(
        &txn,
        pkg.id,
        platform,
        &new_version,
        start_time,
        BuildStates::ENQUEUED_BUILD,
    )
    .await?;
    txn.commit().await?;

    if enqueue_result.inserted {
        let _ = tx.send(Action::Build(
            Box::from(pkg),
            Box::from(enqueue_result.build.clone()),
        ));
    }
    Ok(enqueue_result)
}

#[cfg(test)]
mod tests {
    use super::package_update_with_client;
    use crate::snapshot::SnapshotStore;
    use aurcache_db::migration::Migrator;
    use aurcache_db::packages::SourceData;
    use aurcache_db::prelude::{Dependencies, Packages};
    use aurcache_db::{builds, dependencies, packages};
    use aurcache_deps::AurClient;
    use aurcache_types::builder::{Action, BuildStates};
    use git2::{Repository, Signature};
    use pacman_mirrors::platforms::Platform;
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, Database, EntityTrait, PaginatorTrait, QueryFilter, Set,
        TryIntoModel,
    };
    use sea_orm_migration::MigratorTrait;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    fn rpc_deps_json(
        name: &str,
        pkgbase: &str,
        depends: &[&str],
        make_depends: &[&str],
        version: &str,
    ) -> serde_json::Value {
        json!({
            "Name": name,
            "Version": version,
            "PackageBase": pkgbase,
            "PackageBaseID": 0,
            "ID": 0,
            "NumVotes": 0,
            "Popularity": 0.0,
            "FirstSubmitted": 0,
            "LastModified": 0,
            "URLPath": null,
            "Description": null,
            "Maintainer": null,
            "URL": null,
            "OutOfDate": null,
            "Depends": depends,
            "MakeDepends": make_depends,
            "OptDepends": null,
            "CheckDepends": null,
            "Conflicts": null,
            "Provides": null,
            "Replaces": null,
            "Groups": null,
            "License": null,
            "Keywords": null,
        })
    }

    fn multiinfo_json(results: Vec<serde_json::Value>) -> serde_json::Value {
        json!({
            "type": "multiinfo",
            "resultcount": results.len(),
            "results": results,
        })
    }

    fn make_srcinfo(pkgbase: &str, version: &str, depends: &[&str]) -> String {
        let dep_lines = depends
            .iter()
            .map(|d| format!("    depends = {d}\n"))
            .collect::<String>();
        format!(
            "pkgbase = {pkgbase}\n\
             pkgver = {version}\n\
             pkgrel = 1\n\
             arch = x86_64\n\
             {dep_lines}\n\
             pkgname = {pkgbase}\n"
        )
    }

    /// Create a local bare-ish git repository at `aur_root/{pkgbase}.git`
    /// containing a PKGBUILD + .SRCINFO, standing in for the real AUR git
    /// remote (`https://aur.archlinux.org/{pkgbase}.git`) in tests. Returns
    /// the repo's filesystem path, usable directly as a git remote URL.
    fn create_aur_git_repo(
        aur_root: &Path,
        pkgbase: &str,
        version: &str,
        depends: &[&str],
    ) -> PathBuf {
        let repo_path = aur_root.join(format!("{pkgbase}.git"));
        let repo = Repository::init(&repo_path).unwrap();

        let srcinfo = make_srcinfo(pkgbase, version, depends);
        let depends_arr = depends
            .iter()
            .map(|dep| format!("'{dep}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let pkgbuild = format!(
            "pkgname={pkgbase}\npkgver={version}\npkgrel=1\narch=('x86_64')\ndepends=({depends_arr})\nsource=()\nsha256sums=()\npackage() {{\n  :\n}}\n"
        );

        fs::write(repo_path.join("PKGBUILD"), pkgbuild).unwrap();
        fs::write(repo_path.join(".SRCINFO"), srcinfo).unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.add_path(Path::new(".SRCINFO")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        repo_path
    }

    /// Build a `SnapshotStore` for tests: AUR sources resolve against local
    /// git repos under `aur_root` instead of the real AUR, and checkouts are
    /// kept under a fresh temp dir.
    fn test_store(aur_root: &Path) -> (SnapshotStore, tempfile::TempDir) {
        let checkout_dir = tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root_and_aur_base(
            checkout_dir.path().to_path_buf(),
            aur_root.to_string_lossy().to_string(),
        );
        (store, checkout_dir)
    }

    fn git_pkgbuild(version: &str, depends: &[&str]) -> String {
        let depends = depends
            .iter()
            .map(|dep| format!("'{dep}'"))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "pkgname=git-parent\npkgver={version}\npkgrel=1\narch=('x86_64')\ndepends=({depends})\nsource=()\nsha256sums=()\npackage() {{\n  :\n}}\n"
        )
    }

    fn git_srcinfo(version: &str, depends: &[&str]) -> String {
        let depends = depends
            .iter()
            .map(|dep| format!("    depends = {dep}\n"))
            .collect::<String>();
        format!(
            "pkgbase = git-parent\n    pkgver = {version}\n    pkgrel = 1\n    arch = x86_64\n{depends}\npkgname = git-parent\n"
        )
    }

    fn commit_pkgbuild(repo: &Repository, message: &str, version: &str, depends: &[&str]) {
        fs::write(
            repo.workdir().unwrap().join("PKGBUILD"),
            git_pkgbuild(version, depends),
        )
        .unwrap();
        fs::write(
            repo.workdir().unwrap().join(".SRCINFO"),
            git_srcinfo(version, depends),
        )
        .unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.add_path(Path::new(".SRCINFO")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        let parents = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| repo.find_commit(oid).ok())
            .map(|commit| vec![commit])
            .unwrap_or_default();
        let parent_refs = parents.iter().collect::<Vec<_>>();
        repo.commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            message,
            &tree,
            &parent_refs,
        )
        .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(None).unwrap();
    }

    #[tokio::test]
    async fn package_update_queues_dependency_builds_before_parent_when_constraints_tighten() {
        let server = MockServer::start().await;
        let client = AurClient::with_urls(format!("{}/rpc/v5", server.uri()));
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "parent", "2.0.0", &["child>=2.0"]);
        create_aur_git_repo(aur_root.path(), "child", "2.0.0", &[]);

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "child"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "child",
                    "child",
                    &[],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "child".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(child.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let (store, _checkout_dir) = test_store(aur_root.path());
        let results = package_update_with_client(&client, &store, &db, parent.clone(), false, &tx)
            .await
            .unwrap();

        assert!(
            results.iter().all(|r| !r.enqueued),
            "parent should wait for dependency rebuild"
        );

        let updated_dep = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(parent.id))
            .filter(dependencies::Column::DependeeId.eq(child.id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated_dep.version_constraint, ">=2.0");

        let parent_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            parent_builds.len(),
            1,
            "parent should have a WAITING_FOR_DEPS build while dependency rebuilds"
        );
        assert_eq!(
            parent_builds[0].status,
            Some(BuildStates::WAITING_FOR_DEPS),
            "parent build should be WAITING_FOR_DEPS"
        );

        let child_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            child_builds, 2,
            "dependency should get a new rebuild queued"
        );

        let parent_after = Packages::find_by_id(parent.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(parent_after.status, BuildStates::WAITING_FOR_DEPS);
        assert_eq!(parent_after.upstream_version.as_deref(), Some("2.0.0-1"));
    }

    #[tokio::test]
    async fn package_update_does_not_queue_non_leaf_dependency_builds() {
        let server = MockServer::start().await;
        let client = AurClient::with_urls(format!("{}/rpc/v5", server.uri()));
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "parent", "2.0.0", &["child>=2.0"]);
        create_aur_git_repo(aur_root.path(), "child", "2.0.0", &["grandchild>=2.0"]);
        create_aur_git_repo(aur_root.path(), "grandchild", "2.0.0", &[]);

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "child"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "child",
                    "child",
                    &["grandchild>=2.0"],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "grandchild"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "grandchild",
                    "grandchild",
                    &[],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "child".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let grandchild = packages::ActiveModel {
            name: Set("grandchild".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "grandchild".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(child.id),
            dependee_id: Set(grandchild.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(child.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(grandchild.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let (store, _checkout_dir) = test_store(aur_root.path());
        let results = package_update_with_client(&client, &store, &db, parent.clone(), false, &tx)
            .await
            .unwrap();

        assert!(
            results.iter().all(|r| !r.enqueued),
            "parent should wait for transitive dependency rebuilds"
        );

        let parent_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            parent_builds.len(),
            1,
            "parent should have a WAITING_FOR_DEPS build while transitive dependency rebuilds"
        );
        assert_eq!(
            parent_builds[0].status,
            Some(BuildStates::WAITING_FOR_DEPS),
            "parent build should be WAITING_FOR_DEPS"
        );

        let child_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            child_builds.len(),
            2,
            "non-leaf dependency should get a WAITING_FOR_DEPS build while grandchild rebuilds"
        );
        let child_pending = child_builds
            .iter()
            .find(|b| b.status == Some(BuildStates::WAITING_FOR_DEPS));
        assert!(
            child_pending.is_some(),
            "child should have a WAITING_FOR_DEPS build"
        );

        let grandchild_build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(grandchild.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            grandchild_build_count, 2,
            "leaf transitive dependency should be queued first"
        );

        let child_after = Packages::find_by_id(child.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child_after.status, BuildStates::WAITING_FOR_DEPS);
        assert_eq!(child_after.upstream_version.as_deref(), Some("2.0.0-1"));
    }

    #[tokio::test]
    async fn force_rebuild_does_not_queue_non_leaf_dependency_builds() {
        let server = MockServer::start().await;
        let client = AurClient::with_urls(format!("{}/rpc/v5", server.uri()));
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "parent", "2.0.0", &["child>=2.0"]);
        create_aur_git_repo(aur_root.path(), "child", "2.0.0", &["grandchild>=2.0"]);
        create_aur_git_repo(aur_root.path(), "grandchild", "2.0.0", &[]);

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "child"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "child",
                    "child",
                    &["grandchild>=2.0"],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "grandchild"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "grandchild",
                    "grandchild",
                    &[],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "child".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let grandchild = packages::ActiveModel {
            name: Set("grandchild".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "grandchild".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(child.id),
            dependee_id: Set(grandchild.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(child.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(grandchild.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let (store, _checkout_dir) = test_store(aur_root.path());
        let results = package_update_with_client(&client, &store, &db, parent.clone(), true, &tx)
            .await
            .unwrap();

        assert!(
            results.iter().all(|r| !r.enqueued),
            "forced rebuild should still wait for transitive dependency rebuilds"
        );

        let parent_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            parent_builds.len(),
            1,
            "forced rebuild should insert a WAITING_FOR_DEPS build while transitive deps rebuild"
        );
        assert_eq!(
            parent_builds[0].status,
            Some(BuildStates::WAITING_FOR_DEPS),
            "parent build should be WAITING_FOR_DEPS"
        );

        let child_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            child_builds.len(),
            2,
            "forced rebuild should give non-leaf dependency a WAITING_FOR_DEPS build"
        );
        let child_pending = child_builds
            .iter()
            .find(|b| b.status == Some(BuildStates::WAITING_FOR_DEPS));
        assert!(
            child_pending.is_some(),
            "child should have a WAITING_FOR_DEPS build during forced rebuild"
        );

        let grandchild_build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(grandchild.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            grandchild_build_count, 2,
            "forced rebuild should enqueue only the leaf transitive dependency first"
        );
    }

    #[tokio::test]
    async fn git_update_refreshes_dependency_rows() {
        let server = MockServer::start().await;
        let client = AurClient::with_urls(format!("{}/rpc/v5", server.uri()));
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "new-dep"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "new-dep",
                    "new-dep",
                    &[],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        commit_pkgbuild(&repo, "initial", "1.0.0", &["old-dep>=1.0"]);

        let parent = packages::ActiveModel {
            name: Set("git-parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Git),
            source_data: Set(packages::SourceData::Git {
                spec: packages::GitSourceSpec {
                    url: dir.path().to_string_lossy().to_string(),
                    r#ref: "main".to_string(),
                    subfolder: ".".to_string(),
                },
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let old_dep = packages::ActiveModel {
            name: Set("old-dep".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "old-dep".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let new_dep = packages::ActiveModel {
            name: Set("new-dep".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("2.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "new-dep".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(old_dep.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(parent.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(new_dep.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("2.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        commit_pkgbuild(&repo, "updated", "2.0.0", &["new-dep>=2.0"]);

        let checkout_dir = tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root(checkout_dir.path().to_path_buf());
        let build_ids =
            package_update_with_client(&client, &store, &db, parent.clone(), false, &tx)
                .await
                .unwrap();

        assert_eq!(
            build_ids.len(),
            1,
            "parent should enqueue once deps are refreshed"
        );

        let deps = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(deps.len(), 1, "stale git dependency rows should be removed");
        assert_eq!(deps[0].dependee_id, new_dep.id);
        assert_eq!(deps[0].version_constraint, ">=2.0");

        let parent_after = Packages::find_by_id(parent.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(parent_after.upstream_version.as_deref(), Some("2.0.0-1"));
    }

    #[tokio::test]
    async fn force_rebuild_after_failure_queues_new_build() {
        let server = MockServer::start().await;
        let client = AurClient::with_urls(format!("{}/rpc/v5", server.uri()));
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, mut rx) = tokio::sync::broadcast::channel::<Action>(100);

        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "mypkg", "1.0.0", &[]);

        // Simulate package that previously failed its first build.
        let pkg = packages::ActiveModel {
            name: Set("mypkg".to_string()),
            status: Set(BuildStates::FAILED_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "mypkg".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        // The first (failed) build record.
        builds::ActiveModel {
            pkg_id: Set(pkg.id),
            output: Set(None),
            status: Set(Some(BuildStates::FAILED_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        // Drain any stale messages before the force-rebuild call.
        while rx.try_recv().is_ok() {}

        let (store, _checkout_dir) = test_store(aur_root.path());
        let build_ids = package_update_with_client(&client, &store, &db, pkg.clone(), true, &tx)
            .await
            .unwrap();

        assert_eq!(
            build_ids.len(),
            1,
            "force rebuild should queue exactly one build"
        );

        // Verify Action::Build was sent.
        assert!(
            rx.try_recv().is_ok(),
            "Action::Build should have been sent on the channel"
        );

        // Verify the new build row exists with ENQUEUED status.
        let enqueued_build = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(pkg.id))
            .filter(builds::Column::Status.eq(Some(BuildStates::ENQUEUED_BUILD)))
            .one(&db)
            .await
            .unwrap();
        assert!(
            enqueued_build.is_some(),
            "a new ENQUEUED build row should exist after force rebuild"
        );

        // The total build count should be 2 (the original failed + new enqueued).
        let total_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(pkg.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(total_builds, 2, "there should be 2 build records total");
    }

    #[tokio::test]
    async fn update_removes_orphaned_dependency_package() {
        let server = MockServer::start().await;
        let client = AurClient::with_urls(format!("{}/rpc/v5", server.uri()));
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        // A v2.0.0 no longer depends on B
        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "parent", "2.0.0", &[]);

        // Insert parent (directly requested, with a successful build)
        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        // Insert child (not directly requested)
        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "child".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        // Dependency link: parent -> child
        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        // Successful build record for child
        builds::ActiveModel {
            pkg_id: Set(child.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let (store, _checkout_dir) = test_store(aur_root.path());
        package_update_with_client(&client, &store, &db, parent.clone(), false, &tx)
            .await
            .unwrap();

        // Dependency link should be removed
        let dep_count = dependencies::Entity::find()
            .filter(dependencies::Column::DependentId.eq(parent.id))
            .filter(dependencies::Column::DependeeId.eq(child.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            dep_count, 0,
            "dependency link from parent to child should be removed"
        );

        // Child package should be deleted (no dependents left, not directly requested)
        let child_in_db = packages::Entity::find_by_id(child.id)
            .one(&db)
            .await
            .unwrap();
        assert!(
            child_in_db.is_none(),
            "orphaned child package should be removed from the DB"
        );

        // Build records for the deleted package should also be gone
        let build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            build_count, 0,
            "build records for deleted package should be removed"
        );
    }
}
