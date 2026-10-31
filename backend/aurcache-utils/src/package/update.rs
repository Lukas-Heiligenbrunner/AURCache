use crate::package::add::{insert_dependency, provides_json, split_packages_json};
use crate::package::edges::{Declaration, resolve_edges};
use crate::services::Services;
use crate::vcs_check::{SourcesMoved, resolve_vcs_commits, vcs_sources_moved};
use anyhow::{anyhow, bail};
use async_recursion::async_recursion;
use aurcache_activitylog::events::{Event, QueueCause, RefreshTarget};
use aurcache_common::build_state::BuildState;
use aurcache_common::build_state::BuildTrigger;
use aurcache_db::helpers::build_enqueue::{
    EnqueueBuildResult, Pending, Queued, enqueue_build_if_missing, promote_waiting_build,
};
use aurcache_db::prelude::{Dependencies, PackageVcsSources, Packages};
use aurcache_db::{dependencies, package_vcs_sources, packages};
use aurcache_deps::PkgDeps;
use pacman_mirrors::platforms::Platform;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Set,
};
use std::collections::{HashMap, HashSet};
use tracing::{info, warn};

/// The packages `pkg_id` depends on directly, as its edges stand now.
///
/// Read before its graph is re-synced: those are what a re-sync can orphan,
/// and [`live_check`](crate::package::live_check::live_check) collects what
/// they reach once the new edges are in.
async fn direct_dependees(db: &DatabaseConnection, pkg_id: i32) -> anyhow::Result<Vec<i32>> {
    Ok(Dependencies::find()
        .select_only()
        .column(dependencies::Column::DependeeId)
        .filter(dependencies::Column::DependentId.eq(pkg_id))
        .into_tuple::<i32>()
        .all(db)
        .await?)
}

/// Collect what a re-sync of `pkg_id` left unneeded: what its former direct
/// dependencies reach that no requested package needs, keeping `pkg_id`
/// itself, which is mid-update.
async fn collect_dropped(services: &Services, pkg_id: i32, former: &[i32]) -> anyhow::Result<()> {
    crate::package::live_check::live_check(
        &services.db,
        &services.store,
        &services.repo,
        former,
        &[pkg_id],
    )
    .await
}

/// Update every package currently marked as outdated.
///
/// Only packages whose latest build completed successfully are retriggered.
/// Returns the build IDs enqueued across all updated packages.
///
/// Updates of packages with VCS sources run forced, and only those. A VCS
/// package is flagged when an upstream commit moved without its PKGBUILD's
/// version changing, and the unforced path would refuse exactly that case as
/// "already up to date" -- the version is only computed by `pkgver()` at build
/// time.
///
/// Anything else runs unforced, because for it the same version means nothing
/// new to build. Every update refreshes the source snapshot first and fails
/// loudly when the refresh fails, so a forced update can no longer rebuild a
/// stale version whose success would clear the flag -- the failure keeps it
/// flagged until the source catches up.
pub async fn package_update_all_outdated(services: &Services) -> anyhow::Result<Vec<i32>> {
    let db = &services.db;
    let pkg_models: Vec<packages::Model> = Packages::find()
        .filter(packages::Column::OutOfDate.eq(true))
        .order_by_asc(packages::Column::Name)
        .all(db)
        .await?;
    let activity_log = &services.activity;

    // Which outdated packages track a VCS source, in one query: the loop
    // below used to count per package, a round trip each for a flag that
    // only decides forced-vs-unforced.
    let vcs_tracked: std::collections::HashSet<i32> = match PackageVcsSources::find()
        .select_only()
        .column(package_vcs_sources::Column::PackageId)
        .filter(package_vcs_sources::Column::PackageId.is_in(pkg_models.iter().map(|pkg| pkg.id)))
        .into_tuple()
        .all(db)
        .await
    {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            warn!(
                "Auto update could not list VCS-tracked packages, treating all as untracked: {e}"
            );
            std::collections::HashSet::new()
        }
    };

    let mut ids_total = vec![];
    // One package's failure must not starve the rest. A sourceinfo that no
    // longer applies after an upstream bump, a dependency that cannot be
    // fetched, or a resolution that comes apart mid-way all fail this update,
    // and with `?` the whole pass stopped at the first of them -- silently
    // skipping every package after it, every round, while the offender stayed
    // out of date. Each package is handled on its own and moves on.
    for pkg in pkg_models {
        if pkg.status != BuildState::Successful {
            info!(
                "Package auto update was not triggered for package {} because of prev. build status: {}",
                pkg.name,
                pkg.status.label()
            );
            continue;
        }
        let package_name = pkg.name.clone();
        // A VCS package's version does not change until it is built, so the
        // "already up to date" check would refuse it: the flag gets it past
        // that check. It is still an update -- a new version is being
        // attempted -- and the log entry calls it one.
        let force = vcs_tracked.contains(&pkg.id);
        match package_update(services, pkg, force, BuildTrigger::AutoUpdate).await {
            Ok(results) => {
                // No actor: the auto-updater is the server acting on its own,
                // which is what an entry without a user already says.
                activity_log.emit(queued(
                    &package_name,
                    QueueCause::Update,
                    &results,
                    None,
                    None,
                ));
                ids_total.extend(
                    results
                        .into_iter()
                        .filter(|r| r.dispatch == Dispatch::Enqueued)
                        .map(|r| r.build_id),
                );
            }
            Err(e) => activity_log.emit(Event::UpdateSkipped {
                pkg: package_name.into(),
                error: format!("{e:#}"),
            }),
        }
    }
    Ok(ids_total)
}

/// Queue a build of `pkg_model`'s current upstream version on every platform
/// it is configured for, with any dependency rebuilds that needs.
///
/// Refused when the version is already built, unless `force`. `trigger` is
/// recorded on each build row -- and on the rows of any dependency rebuilt
/// along the way, which is part of the same request.
///
/// Returns one entry per configured platform: the build that was queued or
/// promoted, or left waiting on dependencies.
pub async fn package_update(
    services: &Services,
    pkg_model: packages::Model,
    force: bool,
    trigger: BuildTrigger,
) -> anyhow::Result<Vec<PlatformUpdateResult>> {
    let mut visited = HashSet::new();
    package_update_inner(services, pkg_model, force, trigger, &mut visited).await
}

/// Recompute and persist a package's dependency graph from its current
/// (possibly patched) source, without checking versions or enqueuing builds.
///
/// A patch edit can change `depends`/`makedepends` without necessarily
/// bumping `pkgver`/`pkgrel`, so the dependency graph needs to be kept in
/// sync independently of the regular version-triggered update flow. Existing
/// queued builds are reconciled with the new edges; no new parent builds are created.
pub async fn package_resync_dependencies(
    services: &Services,
    pkg_model: &packages::Model,
) -> anyhow::Result<()> {
    let former = direct_dependees(&services.db, pkg_model.id).await?;
    resync_graph(services, pkg_model).await?;

    // The dependency change may have made some previously-required
    // dependency-only packages no longer needed.
    collect_dropped(services, pkg_model.id, &former).await
}

/// Recompute and persist a package's dependency edges from its current source,
/// adding missing dependencies, removing stale edges and reconciling queued builds.
pub(crate) async fn resync_graph(
    services: &Services,
    pkg_model: &packages::Model,
) -> anyhow::Result<()> {
    let sourceinfo = services
        .store
        .sourceinfo(&pkg_model.source_data, pkg_model.patch.as_deref())
        .await
        .map_err(|e| anyhow!("Failed to resolve source info: {e:#}"))?;
    let deps = aurcache_deps::deps_from_srcinfo(
        &sourceinfo,
        &crate::pkg::architectures(&pkg_model.platforms),
    );

    sync_dependency_graph(services, pkg_model, &deps).await?;
    Ok(())
}

/// Recursively update a package and its dependencies, enqueuing builds for ready platforms.
#[allow(clippy::double_must_use)]
#[async_recursion]
async fn package_update_inner(
    services: &Services,
    pkg_model: packages::Model,
    force: bool,
    trigger: BuildTrigger,
    visited: &mut HashSet<i32>,
) -> anyhow::Result<Vec<PlatformUpdateResult>> {
    if !visited.insert(pkg_model.id) {
        return Ok(vec![]);
    }

    // Rebuilds used to resolve through the cached source snapshot, which only
    // the periodic version check refreshes: a PKGBUILD bumped upstream after
    // the last check (a pkgrel-only fix, say) rebuilt stale, and nothing about
    // the build said so. Refresh first, so every update -- forced or not --
    // resolves what upstream has now. A refresh failure fails the update
    // rather than building stale: for the auto-updater that is a visible skip
    // retried next pass, and for an operator it names the real problem.
    if let Err(e) = services.store.refresh(&pkg_model.source_data).await {
        services.activity.emit(Event::SourceRefreshFailed {
            pkg: pkg_model.name.as_str().into(),
            target: RefreshTarget::Snapshot,
            error: format!("{e:#}"),
        });
        bail!("Could not refresh sources, not building from a stale snapshot: {e:#}");
    }

    let sourceinfo = services
        .store
        .sourceinfo(&pkg_model.source_data, pkg_model.patch.as_deref())
        .await
        .map_err(|e| anyhow!("Failed to resolve source info: {e:#}"))?;
    let pkgbuild = services
        .store
        .pkgbuild(&pkg_model.source_data, pkg_model.patch.as_deref())
        .await
        .map_err(|e| anyhow!("Failed to read the PKGBUILD: {e:#}"))?;
    let upstream_version = sourceinfo.base.version.to_string();
    let deps = aurcache_deps::deps_from_srcinfo(
        &sourceinfo,
        &crate::pkg::architectures(&pkg_model.platforms),
    );

    let former = direct_dependees(&services.db, pkg_model.id).await?;
    let graph = sync_dependency_graph(services, &pkg_model, &deps).await?;

    // With the update, it's possible some dependencies are no longer needed.
    collect_dropped(services, pkg_model.id, &former).await?;

    let queued_commits = resolve_vcs_commits(&sourceinfo, &pkgbuild).await;
    let mut requested = pkg_model.clone();
    if !force {
        let mut needed = Vec::new();
        for &platform in pkg_model.platforms.as_slice() {
            let moved =
                vcs_sources_moved(&services.db, pkg_model.id, platform, &queued_commits).await?;
            let current = match moved {
                SourcesMoved::Unmoved => true,
                SourcesMoved::Moved => false,
                SourcesMoved::Unknown => {
                    aurcache_db::helpers::builds::latest_successful_version(
                        &services.db,
                        pkg_model.id,
                        platform,
                    )
                    .await?
                    .as_deref()
                        == Some(upstream_version.as_str())
                }
            };
            if !current {
                needed.push(platform);
            }
        }
        if needed.is_empty() {
            bail!(
                "Latest build is already up to date on every configured platform (version {upstream_version})"
            );
        }
        requested.platforms = aurcache_db::lists::Platforms::new(needed);
    }

    let platform_results = enqueue_platform_builds(
        services,
        BuildRequest {
            pkg_model: &requested,
            version: &upstream_version,
            graph: &graph,
            trigger,
        },
        visited,
    )
    .await?;

    // What these builds are being made from, recorded against each of them, so
    // the next version check compares upstream with what was built rather than
    // with whatever it last happened to look at. Queue time rather than build
    // time: recording the earlier commit errs toward one redundant rebuild
    // rather than a missed one, and a worker that reports what it actually
    // used overwrites this. Best-effort: an unrecorded build reads as unknown
    // later, which costs a redundant rebuild, where failing here would cost
    // the build.
    if !queued_commits.is_empty() {
        for result in &platform_results {
            if let Err(e) = aurcache_db::helpers::builds::record_build_vcs_sources(
                &services.db,
                result.build_id,
                &queued_commits,
            )
            .await
            {
                services.activity.emit(Event::BuildRecordFailed {
                    build: aurcache_common::api::log::BuildRef {
                        pkgbase: pkg_model.name.clone(),
                        number: result.build_number,
                    },
                    what: "queued VCS sources".to_string(),
                    error: e.to_string(),
                });
            }
        }
    }

    let pkgbase = sourceinfo.base.name.to_string();
    // The status is not written here: queueing the builds above brought it in
    // line with them.
    packages::ActiveModel {
        id: Set(pkg_model.id),
        upstream_version: Set(Some(upstream_version.clone())),
        split_packages: Set(split_packages_json(&pkgbase, &deps.pkgnames)?),
        provides: Set(provides_json(&deps.provides)?),
        ..Default::default()
    }
    .update(&services.db)
    .await?;

    Ok(platform_results)
}

/// A single dependency of the package being updated, with its merged bounds
/// (several when a range was declared).
struct DepInfo {
    constraint: Vec<crate::pkg::Constraint>,
    package: packages::Model,
}

/// Dependencies resolved for the current update, keyed by pkgbase.
struct DependencyGraph {
    deps: HashMap<String, DepInfo>,
}

/// Resolve dependency constraints, ensure all dependees exist in the DB,
/// and sync the dependency rows.
///
/// Reconcile pending builds with the new edges in the same transaction.
async fn sync_dependency_graph(
    services: &Services,
    pkg_model: &packages::Model,
    deps: &PkgDeps,
) -> anyhow::Result<DependencyGraph> {
    let PreparedGraph {
        edges: dep_constraints_by_pkgbase,
        packages: dep_packages,
    } = prepare_graph(services, pkg_model, deps).await?;
    let txn = aurcache_db::helpers::txn::begin_write(&services.db).await?;
    sync_dependency_rows(
        &txn,
        pkg_model.id,
        &dep_constraints_by_pkgbase,
        &dep_packages,
    )
    .await?;
    let unblocked = crate::worker_complete::resync_pending_builds(&txn, pkg_model.id).await?;
    txn.commit().await?;
    announce_unblocked(services, &unblocked).await;

    // The prepared map contains every resolved dependency.
    let deps_map = dep_constraints_by_pkgbase
        .into_iter()
        .filter_map(|(pkgbase, constraint)| {
            dep_packages.get(&pkgbase).map(|package| {
                (
                    pkgbase,
                    DepInfo {
                        constraint,
                        package: package.clone(),
                    },
                )
            })
        })
        .collect();

    Ok(DependencyGraph { deps: deps_map })
}

/// A package's resolved dependency edges, with every dependee's row.
struct PreparedGraph {
    /// The constraints on each dependee, keyed by its pkgbase.
    edges: HashMap<String, Vec<crate::pkg::Constraint>>,
    /// The row of each dependee in `edges`, by the same key.
    packages: HashMap<String, packages::Model>,
}

async fn prepare_graph(
    services: &Services,
    package: &packages::Model,
    deps: &PkgDeps,
) -> anyhow::Result<PreparedGraph> {
    let edges = resolve_dependency_edges(services, package, deps).await?;
    ensure_missing_dependency_packages(services, package, &edges).await?;
    let packages = fetch_dep_packages_map(&services.db, &edges).await?;
    if packages.len() != edges.len() {
        bail!("a dependency disappeared during resolution; retry the update");
    }
    Ok(PreparedGraph { edges, packages })
}

async fn announce_unblocked(services: &Services, ids: &[i32]) {
    match aurcache_db::helpers::builds::build_refs(&services.db, ids).await {
        Ok(builds) => {
            for build in builds {
                services
                    .activity
                    .emit(Event::BuildUnblocked { build, by: None });
            }
        }
        Err(error) => warn!("could not describe unblocked builds: {error}"),
    }
}

/// Resolve an edit before committing its configuration, edges and queue state.
/// Source fetching and dependency insertion happen before the write transaction.
/// Orphan cleanup is best effort after commit; it must not report a saved edit as failed.
pub async fn edit_package(
    services: &Services,
    original: &packages::Model,
    proposed: &packages::Model,
) -> anyhow::Result<()> {
    let graph_changed =
        original.patch != proposed.patch || original.platforms != proposed.platforms;
    let former = direct_dependees(&services.db, original.id).await?;
    let graph = if graph_changed {
        let sourceinfo = services
            .store
            .sourceinfo(&proposed.source_data, proposed.patch.as_deref())
            .await?;
        let deps = aurcache_deps::deps_from_srcinfo(
            &sourceinfo,
            &crate::pkg::architectures(&proposed.platforms),
        );
        Some(prepare_graph(services, proposed, &deps).await?)
    } else {
        None
    };
    let txn = aurcache_db::helpers::txn::begin_write(&services.db).await?;
    let current = Packages::find_by_id(original.id)
        .lock_exclusive()
        .one(&txn)
        .await?
        .ok_or_else(|| anyhow!("package was removed during the edit"))?;
    if current.patch != original.patch
        || current.platforms != original.platforms
        || current.build_flags != original.build_flags
    {
        bail!("package configuration changed during the edit; reload and retry");
    }
    packages::ActiveModel {
        id: Set(original.id),
        patch: Set(proposed.patch.clone()),
        platforms: Set(proposed.platforms.clone()),
        build_flags: Set(proposed.build_flags.clone()),
        ..Default::default()
    }
    .update(&txn)
    .await?;
    let mut unblocked = Vec::new();
    if let Some(PreparedGraph { edges, packages }) = graph {
        sync_dependency_rows(&txn, original.id, &edges, &packages).await?;
        unblocked = crate::worker_complete::resync_pending_builds(&txn, original.id).await?;
    }
    aurcache_db::helpers::builds::refresh_package_status(&txn, original.id).await?;
    txn.commit().await?;
    announce_unblocked(services, &unblocked).await;
    if graph_changed && let Err(error) = collect_dropped(services, original.id, &former).await {
        warn!("package edit saved, but orphan cleanup failed: {error:#}");
    }
    Ok(())
}

/// The dependency edges `pkg_model` should have, keyed by dependee pkgbase.
///
/// See [`resolve_edges`], which the add path plans its rows from too.
async fn resolve_dependency_edges(
    services: &Services,
    pkg_model: &packages::Model,
    deps: &PkgDeps,
) -> anyhow::Result<HashMap<String, Vec<crate::pkg::Constraint>>> {
    let declared = crate::pkg::DependencySet::of(deps)?;
    // One package, one resolution, so the snapshot lives no longer than this
    // call.
    let tracked =
        aurcache_db::helpers::dependency_resolution::TrackedPackages::load(&services.db).await?;
    let edges = resolve_edges(
        &services.client,
        &tracked,
        &Declaration {
            pkgbase: &pkg_model.name,
            deps: &declared,
            pkgnames: &deps.pkgnames,
            provides: &deps.provides,
        },
        &[],
        &current_dependee_names(&services.db, pkg_model.id).await?,
        Some(&services.activity),
    )
    .await?;
    Ok(edges
        .into_iter()
        .map(|edge| (edge.pkgbase, edge.bounds))
        .collect())
}

/// The package bases this package already depends on.
///
/// Handed back to resolution as a preference, which is what lets a dependency
/// someone repointed by hand persist. Every edge is recomputed from the
/// declared names on each update, so a provider that beat the ranking once
/// would otherwise be replaced by the ranking's own pick the next time round.
/// Being the existing edge is only a tie-break among the packages that
/// genuinely satisfy the name: one that stops providing it is dropped like any
/// other, and the official repositories are still asked first.
async fn current_dependee_names(
    db: &DatabaseConnection,
    dependent_id: i32,
) -> anyhow::Result<HashSet<String>> {
    let dependee_ids: Vec<i32> = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent_id))
        .select_only()
        .column(dependencies::Column::DependeeId)
        .into_tuple()
        .all(db)
        .await?;
    if dependee_ids.is_empty() {
        return Ok(HashSet::new());
    }

    Ok(Packages::find()
        .filter(packages::Column::Id.is_in(dependee_ids))
        .select_only()
        .column(packages::Column::Name)
        .into_tuple::<String>()
        .all(db)
        .await?
        .into_iter()
        .collect())
}

/// Ensure all resolved dependency packages exist in the database,
/// adding them via the AUR if missing.
async fn ensure_missing_dependency_packages(
    services: &Services,
    pkg_model: &packages::Model,
    dep_constraints_by_pkgbase: &HashMap<String, Vec<crate::pkg::Constraint>>,
) -> anyhow::Result<()> {
    // The packages already tracked, in one query — not one probe per
    // declared dependency. Nothing declared means nothing to look up (`IN ()`
    // is not valid SQL everywhere).
    if dep_constraints_by_pkgbase.is_empty() {
        return Ok(());
    }
    let tracked: HashSet<String> = Packages::find()
        .select_only()
        .column(packages::Column::Name)
        .filter(packages::Column::Name.is_in(dep_constraints_by_pkgbase.keys().map(String::as_str)))
        .into_tuple::<String>()
        .all(&services.db)
        .await?
        .into_iter()
        .collect();
    for dep_pkgbase in dep_constraints_by_pkgbase.keys() {
        if !tracked.contains(dep_pkgbase) {
            insert_dependency(services, pkg_model, dep_pkgbase).await?;
        }
    }
    Ok(())
}

/// Fetch a name→model map of all dependency packages from the database.
async fn fetch_dep_packages_map(
    db: &DatabaseConnection,
    dep_constraints_by_pkgbase: &HashMap<String, Vec<crate::pkg::Constraint>>,
) -> anyhow::Result<HashMap<String, packages::Model>> {
    if dep_constraints_by_pkgbase.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(Packages::find()
        .filter(packages::Column::Name.is_in(dep_constraints_by_pkgbase.keys().cloned()))
        .all(db)
        .await?
        .into_iter()
        .map(|pkg| (pkg.name.clone(), pkg))
        .collect())
}

/// Insert, update, or remove dependency rows to match the current constraint set.
async fn sync_dependency_rows<C: ConnectionTrait>(
    db: &C,
    dependent_id: i32,
    dep_constraints_by_pkgbase: &HashMap<String, Vec<crate::pkg::Constraint>>,
    dep_packages: &HashMap<String, packages::Model>,
) -> anyhow::Result<()> {
    let desired_dependee_ids: HashSet<i32> = dep_packages.values().map(|pkg| pkg.id).collect();

    // Loaded once and shared by both passes below: the rows the upsert pass
    // needs are already here, so re-querying each edge is pure overhead.
    let existing: HashMap<i32, dependencies::Model> = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent_id))
        .all(db)
        .await?
        .into_iter()
        .map(|row| (row.dependee_id, row))
        .collect();

    let stale: Vec<i32> = existing
        .keys()
        .filter(|id| !desired_dependee_ids.contains(id))
        .copied()
        .collect();
    if !stale.is_empty() {
        Dependencies::delete_many()
            .filter(dependencies::Column::DependentId.eq(dependent_id))
            .filter(dependencies::Column::DependeeId.is_in(stale))
            .exec(db)
            .await?;
    }

    for (dep_pkgbase, constraint) in dep_constraints_by_pkgbase {
        let Some(dep_pkg) = dep_packages.get(dep_pkgbase) else {
            continue;
        };
        let serialized = crate::pkg::join_constraints(constraint);

        if let Some(edge) = existing.get(&dep_pkg.id) {
            let mut active: dependencies::ActiveModel = edge.clone().into();
            active.version_constraint = Set(serialized.clone());
            active.save(db).await?;
        } else {
            dependencies::ActiveModel {
                dependent_id: Set(dependent_id),
                dependee_id: Set(dep_pkg.id),
                version_constraint: Set(serialized),
                ..Default::default()
            }
            .save(db)
            .await?;
        }
    }

    Ok(())
}

/// Queue missing dependency builds and choose the dependent's initial queue state.
///
/// Every dependency not yet satisfied gets a build of its own if it has none
/// pending, so it will be available when the dependent starts -- all of them,
/// not only the first: the dependent is promoted once *every* dependency is
/// satisfied, so one left without a build would hold it back for ever.
/// Packages whose last build failed are never auto-retriggered — the user has
/// to retry those explicitly.
async fn prepare_dependencies_for_platform(
    services: &Services,
    platform: Platform,
    graph: &DependencyGraph,
    trigger: BuildTrigger,
    visited: &mut HashSet<i32>,
    needed_by: &str,
) -> anyhow::Result<Pending> {
    let mut pending = Pending::Enqueued;
    for dep_info in graph.deps.values() {
        // The same check the builder makes when this dependency's build
        // lands, against the constraint as the edge stores it.
        if aurcache_db::helpers::builds::dependency_satisfied(
            &services.db,
            dep_info.package.id,
            platform,
            &crate::pkg::join_constraints(&dep_info.constraint),
        )
        .await?
        {
            continue;
        }
        pending = Pending::WaitingForDeps;

        let has_pending_build = aurcache_db::helpers::builds::pending_build(
            &services.db,
            dep_info.package.id,
            platform,
        )
        .await?
        .is_some();

        // A dependency whose last build failed is not auto-retried.
        if !has_pending_build && dep_info.package.status != BuildState::Failed {
            let results =
                package_update_inner(services, dep_info.package.clone(), true, trigger, visited)
                    .await?;
            if !results.is_empty() {
                services.activity.emit(queued(
                    &dep_info.package.name,
                    QueueCause::Dependency,
                    &results,
                    None,
                    Some(needed_by),
                ));
            }
        }
    }

    Ok(pending)
}

/// What to build and its resolved dependency graph.
struct BuildRequest<'a> {
    pkg_model: &'a packages::Model,
    version: &'a str,
    graph: &'a DependencyGraph,
    trigger: BuildTrigger,
}

/// Outcome of triggering an update for a single platform of a package.
#[derive(Debug, Clone)]
pub struct PlatformUpdateResult {
    pub platform: Platform,
    /// The build row created/reused for this platform. Present regardless of
    /// whether the build was actually dispatched or is still waiting on a
    /// dependency rebuild.
    pub build_id: i32,
    /// The same build's public number within its package. Callers that report
    /// back to a user want this rather than `build_id`, which is internal.
    pub build_number: i32,
    /// Where this call left it.
    pub dispatch: Dispatch,
    /// The version the build is of.
    pub version: String,
}

/// Where a platform's update left its build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// Queued for a worker by this call: inserted, or promoted from waiting.
    Enqueued,
    /// A build was already pending, and is left as it was.
    AlreadyPending,
    /// Queued to wait for a dependency that is being rebuilt first.
    WaitingForDeps,
}

impl From<Queued> for Dispatch {
    fn from(queued: Queued) -> Self {
        match queued {
            Queued::Inserted | Queued::Promoted => Self::Enqueued,
            Queued::Existing => Self::AlreadyPending,
        }
    }
}

/// The log entry for builds `results` queued for `pkgbase`, and why.
///
/// The version is named for an update only: it is what is new there, where a
/// rebuild or a retry repeats the version that was already built.
#[must_use]
pub fn queued(
    pkgbase: &str,
    cause: QueueCause,
    results: &[PlatformUpdateResult],
    retried: Option<aurcache_common::api::log::BuildRef>,
    needed_by: Option<&str>,
) -> Event {
    Event::BuildQueued {
        pkg: pkgbase.into(),
        cause,
        builds: results
            .iter()
            .map(|result| aurcache_common::api::log::BuildRef {
                pkgbase: pkgbase.to_string(),
                number: result.build_number,
            })
            .collect(),
        version: (cause == QueueCause::Update)
            .then(|| results.first().map(|result| result.version.clone()))
            .flatten()
            .filter(|version| !version.is_empty()),
        retried,
        needed_by: needed_by.map(Into::into),
    }
}

/// For each configured platform, check dep readiness and enqueue builds.
async fn enqueue_platform_builds(
    services: &Services,
    request: BuildRequest<'_>,
    visited: &mut HashSet<i32>,
) -> anyhow::Result<Vec<PlatformUpdateResult>> {
    let mut results = Vec::new();

    for &platform in request.pkg_model.platforms.as_slice() {
        let pending = prepare_dependencies_for_platform(
            services,
            platform,
            request.graph,
            request.trigger,
            visited,
            &request.pkg_model.name,
        )
        .await?;

        let result = match pending {
            Pending::Enqueued => {
                update_platform(
                    &services.db,
                    request.pkg_model.id,
                    platform,
                    request.version,
                    request.trigger,
                )
                .await?
            }
            Pending::WaitingForDeps => {
                enqueue_build_if_missing(
                    &services.db,
                    request.pkg_model.id,
                    platform,
                    request.version,
                    aurcache_db::helpers::time::now_secs(),
                    pending,
                    request.trigger,
                )
                .await?
            }
        };
        results.push(PlatformUpdateResult {
            platform,
            build_id: result.build.id,
            build_number: result.build.number,
            dispatch: match (pending, result.queued) {
                (Pending::WaitingForDeps, Queued::Inserted) => Dispatch::WaitingForDeps,
                (_, queued) => queued.into(),
            },
            version: request.version.to_string(),
        });
    }

    Ok(results)
}

/// Create or reuse the pending build entry for package `pkg_id` on one platform.
///
/// If a `WAITING_FOR_DEPS` build already exists for this `(pkg, platform)`, it is promoted to
/// `ENQUEUED` rather than inserting a duplicate.  This happens when a dependency
/// finishes and the dependent was already in the pending queue waiting for it.
async fn update_platform(
    db: &DatabaseConnection,
    pkg_id: i32,
    platform: Platform,
    new_version: &str,
    trigger: BuildTrigger,
) -> anyhow::Result<EnqueueBuildResult> {
    if let Some(promoted) = promote_waiting_build(db, pkg_id, platform).await? {
        return Ok(EnqueueBuildResult {
            build: promoted,
            queued: Queued::Promoted,
        });
    }

    Ok(enqueue_build_if_missing(
        db,
        pkg_id,
        platform,
        new_version,
        aurcache_db::helpers::time::now_secs(),
        Pending::Enqueued,
        trigger,
    )
    .await?)
}

#[cfg(test)]
mod tests {
    use super::package_update;
    use aurcache_activitylog::activity_utils::ActivityLog;
    use aurcache_common::build_state::BuildTrigger;
    /// A repository of its own for a test that never publishes to it.
    fn test_repo() -> Arc<crate::repository::Repository> {
        Arc::new(crate::repository::Repository::new(
            tempdir().unwrap().keep(),
        ))
    }
    use crate::services::Services;
    use crate::snapshot::SnapshotStore;
    use aurcache_common::build_state::BuildState;
    use aurcache_db::migration::Migrator;
    use aurcache_db::packages::SourceData;
    use aurcache_db::prelude::{Dependencies, Packages};
    use aurcache_db::{builds, dependencies, packages};
    use aurcache_deps::AurClient;
    use git2::{Repository, Signature};
    use pacman_mirrors::platforms::Platform;
    use sea_orm::DatabaseConnection;
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, Database, EntityTrait, IntoActiveModel, PaginatorTrait,
        QueryFilter, Set, TryIntoModel,
    };
    use sea_orm_migration::MigratorTrait;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
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

        let srcinfo = make_srcinfo(pkgbase, version, depends);
        let depends_arr = depends
            .iter()
            .map(|dep| format!("'{dep}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let pkgbuild = format!(
            "pkgname={pkgbase}\npkgver={version}\npkgrel=1\narch=('x86_64')\ndepends=({depends_arr})\nsource=()\nsha256sums=()\npackage() {{\n  :\n}}\n"
        );

        crate::test_git::create_repo(
            &repo_path,
            &[("PKGBUILD", &pkgbuild), (".SRCINFO", &srcinfo)],
        );
        repo_path
    }

    /// Move the fake AUR repo at `aur_root/{pkgbase}.git` to a new version,
    /// standing in for an upstream PKGBUILD bump the store has not seen. The
    /// store cache still holds the old snapshot afterwards, which is the point.
    fn bump_aur_git_repo(aur_root: &Path, pkgbase: &str, version: &str, depends: &[&str]) {
        let repo_path = aur_root.join(format!("{pkgbase}.git"));
        let repo = Repository::open(&repo_path).unwrap();
        let depends_arr = depends
            .iter()
            .map(|dep| format!("'{dep}'"))
            .collect::<Vec<_>>()
            .join(" ");
        fs::write(
            repo_path.join("PKGBUILD"),
            format!(
                "pkgname={pkgbase}\npkgver={version}\npkgrel=1\narch=('x86_64')\ndepends=({depends_arr})\nsource=()\nsha256sums=()\npackage() {{\n  :\n}}\n"
            ),
        )
        .unwrap();
        fs::write(
            repo_path.join(".SRCINFO"),
            make_srcinfo(pkgbase, version, depends),
        )
        .unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.add_path(Path::new(".SRCINFO")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "bump", &tree, &[&parent])
            .unwrap();
    }

    /// A store, and the directory its checkouts live in, which goes when this
    /// does.
    struct TestStore {
        store: SnapshotStore,
        _checkouts: tempfile::TempDir,
    }

    /// Build a `SnapshotStore` for tests: AUR sources resolve against local
    /// git repos under `aur_root` instead of the real AUR, and checkouts are
    /// kept under a fresh temp dir.
    fn test_store(aur_root: &Path) -> TestStore {
        let checkout_dir = tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root_and_aur_base(
            checkout_dir.path().to_path_buf(),
            aur_root.to_string_lossy().to_string(),
        );
        TestStore {
            store,
            _checkouts: checkout_dir,
        }
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

    /// A client, and the directory its official repositories are read from,
    /// which goes when this does.
    struct TestClient {
        client: AurClient,
        _repos: tempfile::TempDir,
    }

    /// A client whose official-repo cache is present and empty.
    ///
    /// Resolution asks the official repositories about every dependency, and
    /// an unreadable cache is an error rather than an empty answer -- that is
    /// what stops a mirror outage from sending `git` to the AUR. A test that
    /// wants the repositories to hold nothing therefore has to say so, by
    /// handing over a cache that is present, fresh and empty.
    async fn client_with_empty_official_repos(rpc_url: String) -> TestClient {
        let dir = tempfile::tempdir().unwrap();
        for repo in ["core", "extra", "multilib"] {
            let mut archive = Vec::new();
            {
                let encoder =
                    flate2::write::GzEncoder::new(&mut archive, flate2::Compression::default());
                let mut builder = tar::Builder::new(encoder);
                builder.finish().unwrap();
                builder.into_inner().unwrap().finish().unwrap();
            }
            std::fs::write(dir.path().join(format!("{repo}.db.tar.gz")), &archive).unwrap();
        }
        let client = AurClient::with_urls_and_paths(
            rpc_url,
            // Never read: nothing in the cache is stale, so no download is
            // attempted and no mirror is needed.
            dir.path().join("no-mirrorlist"),
            dir.path().to_path_buf(),
        );
        // Present and empty: "the official repositories hold nothing", as
        // opposed to "they could not be read", which resolution refuses to
        // answer.
        client.official.refresh().await.unwrap();
        TestClient {
            client,
            _repos: dir,
        }
    }

    #[tokio::test]
    async fn package_update_queues_dependency_builds_before_parent_when_constraints_tighten() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let results = package_update(
            &Services::new(
                db.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            parent.clone(),
            false,
            BuildTrigger::User,
        )
        .await
        .unwrap();

        assert!(
            results
                .iter()
                .all(|r| r.dispatch != super::Dispatch::Enqueued),
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
            BuildState::WaitingForDeps,
            "parent build should be WAITING_FOR_DEPS"
        );
        // The request's own trigger, on the build left waiting as much as on
        // one queued outright -- it used to be `AutoUpdate` for everything,
        // which made an operator's rebuild indistinguishable from the scheduler.
        assert_eq!(parent_builds[0].trigger, BuildTrigger::User);

        let child_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            child_builds.len(),
            2,
            "dependency should get a new rebuild queued"
        );
        let rebuild = child_builds
            .iter()
            .find(|b| b.status != BuildState::Successful)
            .expect("the dependency's new build");
        assert_eq!(
            rebuild.trigger,
            BuildTrigger::User,
            "a dependency rebuilt for the request is part of the request"
        );

        let parent_after = Packages::find_by_id(parent.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(parent_after.status, BuildState::WaitingForDeps);
        assert_eq!(parent_after.upstream_version.as_deref(), Some("2.0.0-1"));
    }

    /// A package built at 1.0, depended on by whatever the test links to it.
    async fn built_package(
        db: &DatabaseConnection,
        name: &str,
        requested: bool,
    ) -> packages::Model {
        let pkg = packages::ActiveModel {
            name: Set(name.to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur { name: name.into() }),
            directly_requested: Set(requested),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
        builds::ActiveModel {
            pkg_id: Set(pkg.id),
            status: Set(BuildState::Successful),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
        pkg
    }

    /// Every dependency the update leaves unsatisfied gets a build, not only
    /// the first one found: the parent is promoted once all of them are
    /// satisfied, so a second one left unbuilt would hold it back for ever.
    #[tokio::test]
    async fn package_update_queues_every_unsatisfied_dependency() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let aur_root = tempdir().unwrap();
        create_aur_git_repo(
            aur_root.path(),
            "parent",
            "2.0.0",
            &["one>=2.0", "two>=2.0"],
        );
        create_aur_git_repo(aur_root.path(), "one", "2.0.0", &[]);
        create_aur_git_repo(aur_root.path(), "two", "2.0.0", &[]);
        for name in ["one", "two"] {
            Mock::given(method("GET"))
                .and(path("/rpc/v5/info"))
                .and(query_param("arg[]", name))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                        name,
                        name,
                        &[],
                        &[],
                        "2.0.0",
                    )])),
                )
                .mount(&server)
                .await;
        }

        let parent = built_package(&db, "parent", true).await;
        let mut dependees = vec![];
        for name in ["one", "two"] {
            let dependee = built_package(&db, name, false).await;
            dependencies::ActiveModel {
                dependent_id: Set(parent.id),
                dependee_id: Set(dependee.id),
                version_constraint: Set(">=1.0".to_string()),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
            dependees.push(dependee);
        }

        let TestStore { store, _checkouts } = test_store(aur_root.path());
        package_update(
            &Services::new(
                db.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            parent,
            false,
            BuildTrigger::User,
        )
        .await
        .unwrap();

        for dependee in dependees {
            let pending = builds::Entity::find()
                .filter(builds::Column::PkgId.eq(dependee.id))
                .filter(builds::Column::Status.ne(BuildState::Successful))
                .count(&db)
                .await
                .unwrap();
            assert_eq!(pending, 1, "{} must get a build of its own", dependee.name);
        }
    }

    #[tokio::test]
    async fn package_update_does_not_queue_non_leaf_dependency_builds() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
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
            status: Set(BuildState::Successful),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let results = package_update(
            &Services::new(
                db.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            parent.clone(),
            false,
            BuildTrigger::User,
        )
        .await
        .unwrap();

        assert!(
            results
                .iter()
                .all(|r| r.dispatch != super::Dispatch::Enqueued),
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
            BuildState::WaitingForDeps,
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
            .find(|b| b.status == BuildState::WaitingForDeps);
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
        assert_eq!(child_after.status, BuildState::WaitingForDeps);
        assert_eq!(child_after.upstream_version.as_deref(), Some("2.0.0-1"));
    }

    #[tokio::test]
    async fn force_rebuild_does_not_queue_non_leaf_dependency_builds() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
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
            status: Set(BuildState::Successful),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let results = package_update(
            &Services::new(
                db.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            parent.clone(),
            true,
            BuildTrigger::User,
        )
        .await
        .unwrap();

        assert!(
            results
                .iter()
                .all(|r| r.dispatch != super::Dispatch::Enqueued),
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
            BuildState::WaitingForDeps,
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
            .find(|b| b.status == BuildState::WaitingForDeps);
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
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("2.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
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
            status: Set(BuildState::Successful),
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
        let build_ids = package_update(
            &Services::new(
                db.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            parent.clone(),
            false,
            BuildTrigger::User,
        )
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

    /// What [`two_providers_of_one_name`] sets up.
    struct TwoProviders {
        parent: packages::Model,
        /// Id of `mydep`, which carries the name.
        by_name: i32,
        /// Id of `mydep-git`, which only provides it.
        by_provides: i32,
    }

    /// Set up a git package declaring `mydep`, with two tracked packages able
    /// to satisfy it: `mydep`, which carries the name, and `mydep-git`, which
    /// only provides it. Ranking prefers the first.
    async fn two_providers_of_one_name(db: &DatabaseConnection, repo_path: &Path) -> TwoProviders {
        let parent = packages::ActiveModel {
            name: Set("git-parent".to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set(Default::default()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(packages::SourceData::Git {
                spec: packages::GitSourceSpec {
                    url: repo_path.to_string_lossy().to_string(),
                    r#ref: "main".to_string(),
                    subfolder: String::new(),
                },
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let provider = async |name: &str, provides: Option<serde_json::Value>| {
            packages::ActiveModel {
                name: Set(name.to_string()),
                status: Set(BuildState::Successful),
                out_of_date: Set(false),
                upstream_version: Set(Some("1.0.0".to_string())),
                build_flags: Set(Default::default()),
                platforms: Set("x86_64".parse().unwrap()),
                source_data: Set(SourceData::Aur { name: name.into() }),
                // Requested, so the loser is not swept as an orphan and the
                // assertion is about the edge and nothing else.
                directly_requested: Set(true),
                split_packages: Set(None),
                provides: Set(provides.map(|value| value.to_string())),
                ..Default::default()
            }
            .save(db)
            .await
            .unwrap()
            .id
            .unwrap()
        };

        let by_name = provider("mydep", None).await;
        let by_provides = provider("mydep-git", Some(json!(["mydep"]))).await;
        TwoProviders {
            parent,
            by_name,
            by_provides,
        }
    }

    /// A dependency someone repointed by hand survives re-resolution.
    ///
    /// Every edge is recomputed from the declared names, so the existing edge
    /// has to be an input to that or the ranking takes the choice straight
    /// back: `mydep` carries the name and `mydep-git` only provides it.
    #[tokio::test]
    async fn a_repointed_dependency_survives_a_resync() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        commit_pkgbuild(&repo, "initial", "1.0.0", &["mydep"]);

        let TwoProviders {
            parent,
            by_provides,
            ..
        } = two_providers_of_one_name(&db, dir.path()).await;

        // The hand-picked edge, as an "update this link" action would leave it.
        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(by_provides),
            version_constraint: Set(String::new()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let checkout_dir = tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root(checkout_dir.path().to_path_buf());
        super::package_resync_dependencies(
            &Services::new(
                db.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            &parent,
        )
        .await
        .unwrap();

        let deps = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(
            deps[0].dependee_id, by_provides,
            "the existing edge should outrank the better-ranked candidate"
        );
    }

    /// The control: with no edge to prefer, the same two candidates resolve
    /// the other way round. Without this the test above would pass on a
    /// resolver that simply never changed its mind.
    #[tokio::test]
    async fn ranking_picks_the_provider_when_there_is_no_edge_yet() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        commit_pkgbuild(&repo, "initial", "1.0.0", &["mydep"]);

        let TwoProviders {
            parent, by_name, ..
        } = two_providers_of_one_name(&db, dir.path()).await;

        let checkout_dir = tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root(checkout_dir.path().to_path_buf());
        super::package_resync_dependencies(
            &Services::new(
                db.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            &parent,
        )
        .await
        .unwrap();

        let deps = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].dependee_id, by_name);
    }

    #[tokio::test]
    async fn force_rebuild_after_failure_queues_new_build() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "mypkg", "1.0.0", &[]);

        // Simulate package that previously failed its first build.
        let pkg = packages::ActiveModel {
            name: Set("mypkg".to_string()),
            status: Set(BuildState::Failed),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Failed),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let build_ids = package_update(
            &Services::new(
                db.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            pkg.clone(),
            true,
            BuildTrigger::User,
        )
        .await
        .unwrap();

        assert_eq!(
            build_ids.len(),
            1,
            "force rebuild should queue exactly one build"
        );

        // Verify the new build row exists with ENQUEUED status.
        let enqueued_build = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(pkg.id))
            .filter(builds::Column::Status.eq(BuildState::Enqueued))
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

    /// A forced rebuild resolves what upstream has *now*, not what the last
    /// version check cached: the update refreshes the source snapshot first,
    /// so a PKGBUILD bumped after the last check (a pkgrel-only fix, say)
    /// builds at the new version instead of silently rebuilding the old one.
    #[tokio::test]
    async fn forced_rebuild_refreshes_the_snapshot_before_building() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "mypkg", "1.0.0", &[]);

        let pkg = packages::ActiveModel {
            name: Set("mypkg".to_string()),
            status: Set(BuildState::Failed),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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

        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let services = Services::new(
            db.clone(),
            Arc::new(store),
            Arc::new(client),
            test_repo(),
            ActivityLog::discarding(),
        );

        // Prime the cache: the first rebuild resolves and remembers 1.0.0.
        package_update(&services, pkg.clone(), true, BuildTrigger::User)
            .await
            .unwrap();

        // Upstream moves, with no version check in between to notice.
        // That build fails, as the reported scenario's first attempt did: a
        // still-pending build would be reused as-is instead of re-resolved.
        for build in builds::Entity::find()
            .filter(builds::Column::PkgId.eq(pkg.id))
            .all(&db)
            .await
            .unwrap()
        {
            let mut active = build.into_active_model();
            active.status = Set(BuildState::Failed);
            active.update(&db).await.unwrap();
        }

        // Upstream moves, with no version check in between to notice.
        bump_aur_git_repo(aur_root.path(), "mypkg", "1.0.1", &[]);

        // The rebuild must pick up 1.0.1, not requeue the cached 1.0.0.
        package_update(&services, pkg.clone(), true, BuildTrigger::User)
            .await
            .unwrap();

        let versions: Vec<String> = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(pkg.id))
            .filter(builds::Column::Status.eq(BuildState::Enqueued))
            .all(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|b| b.version)
            .collect();
        assert!(
            versions.iter().any(|v| v == "1.0.1-1"),
            "a rebuild after an upstream bump should enqueue the new version, got: {versions:?}"
        );
    }

    /// Auto-update rebuilds an unchanged version only for a package that tracks
    /// VCS sources. For anything else the flag with no newer source is a
    /// snapshot that has not caught up, and forcing it would rebuild the same
    /// version on every pass.
    #[tokio::test]
    async fn auto_update_forces_only_packages_with_vcs_sources() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let aur_root = tempdir().unwrap();
        let mut ids = std::collections::HashMap::new();
        for name in ["plain", "tracks-git"] {
            create_aur_git_repo(aur_root.path(), name, "1.0.0", &[]);
            // Built at the version its source still reports, and flagged anyway.
            let pkg = packages::ActiveModel {
                name: Set(name.to_string()),
                status: Set(BuildState::Successful),
                out_of_date: Set(true),
                upstream_version: Set(Some("1.0.0-1".to_string())),
                build_flags: Set(Default::default()),
                platforms: Set("x86_64".parse().unwrap()),
                source_data: Set(SourceData::Aur { name: name.into() }),
                directly_requested: Set(true),
                split_packages: Set(None),
                ..Default::default()
            }
            .save(&db)
            .await
            .unwrap()
            .try_into_model()
            .unwrap();
            builds::ActiveModel {
                pkg_id: Set(pkg.id),
                status: Set(BuildState::Successful),
                start_time: Set(Some(1)),
                end_time: Set(Some(2)),
                platform: Set(Platform::X86_64),
                version: Set("1.0.0-1".to_string()),
                ..Default::default()
            }
            .save(&db)
            .await
            .unwrap();
            ids.insert(name, pkg.id);
        }
        aurcache_db::package_vcs_sources::ActiveModel {
            package_id: Set(ids["tracks-git"]),
            source_url: Set("git+https://example.com/repo.git".to_string()),
            last_commit: Set("abc".to_string()),
            updated_at: Set(1),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let services = Services::new(
            db.clone(),
            Arc::new(store),
            Arc::new(client),
            test_repo(),
            ActivityLog::discarding(),
        );
        super::package_update_all_outdated(&services).await.unwrap();

        let queued_for = |name: &'static str| {
            let db = db.clone();
            let id = ids[name];
            async move {
                builds::Entity::find()
                    .filter(builds::Column::PkgId.eq(id))
                    .count(&db)
                    .await
                    .unwrap()
                    - 1
            }
        };
        assert_eq!(
            queued_for("tracks-git").await,
            1,
            "the VCS package rebuilds"
        );
        assert_eq!(
            queued_for("plain").await,
            0,
            "an unchanged version with no VCS source is not rebuilt"
        );
        let scheduled = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(ids["tracks-git"]))
            .filter(builds::Column::Status.eq(BuildState::Enqueued))
            .one(&db)
            .await
            .unwrap()
            .expect("the queued rebuild");
        assert_eq!(
            scheduled.trigger,
            BuildTrigger::AutoUpdate,
            "the scheduler's builds say they are the scheduler's"
        );
    }

    #[tokio::test]
    async fn update_removes_orphaned_dependency_package() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        // A v2.0.0 no longer depends on B
        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "parent", "2.0.0", &[]);

        // Insert parent (directly requested, with a successful build)
        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm;--noprogressbar".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
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
            status: Set(BuildState::Successful),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let TestStore { store, _checkouts } = test_store(aur_root.path());
        package_update(
            &Services::new(
                db.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            parent.clone(),
            false,
            BuildTrigger::User,
        )
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

    #[tokio::test]
    async fn unchanged_version_queues_only_platforms_without_a_success() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "multiarch", "1.0.0", &[]);
        let pkg = built_package(&db, "multiarch", true).await;
        builds::Entity::update_many()
            .col_expr(
                builds::Column::Version,
                sea_orm::sea_query::Expr::value("1.0.0-1"),
            )
            .exec(&db)
            .await
            .unwrap();
        let mut model = pkg.into_active_model();
        model.platforms = Set("x86_64;aarch64".parse().unwrap());
        let pkg = model.update(&db).await.unwrap();
        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let services = Services::new(
            db.clone(),
            Arc::new(store),
            Arc::new(client),
            test_repo(),
            ActivityLog::discarding(),
        );
        let results = package_update(&services, pkg, false, BuildTrigger::User)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        let queued = builds::Entity::find_by_id(results[0].build_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queued.platform, Platform::Aarch64);
        assert_eq!(queued.status, BuildState::Enqueued);
    }

    #[tokio::test]
    async fn resync_reconciles_existing_builds_in_both_directions() {
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        commit_pkgbuild(&repo, "initial", "1.0.0", &["mydep"]);
        let TwoProviders { parent, .. } = two_providers_of_one_name(&db, dir.path()).await;
        let pending = builds::ActiveModel {
            pkg_id: Set(parent.id),
            status: Set(BuildState::Enqueued),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0-1".into()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        let checkout = tempdir().unwrap();
        let services = Services::new(
            db.clone(),
            Arc::new(SnapshotStore::with_checkout_root(
                checkout.path().to_path_buf(),
            )),
            Arc::new(client),
            test_repo(),
            ActivityLog::discarding(),
        );
        super::package_resync_dependencies(&services, &parent)
            .await
            .unwrap();
        assert_eq!(
            builds::Entity::find_by_id(pending.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .status,
            BuildState::WaitingForDeps
        );
        commit_pkgbuild(&repo, "remove dependency", "1.0.0", &[]);
        services.store.refresh(&parent.source_data).await.unwrap();
        super::package_resync_dependencies(&services, &parent)
            .await
            .unwrap();
        assert_eq!(
            builds::Entity::find_by_id(pending.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .status,
            BuildState::Enqueued
        );
        assert_eq!(
            Packages::find_by_id(parent.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .status,
            BuildState::Enqueued
        );
    }

    #[tokio::test]
    async fn failed_edit_keeps_configuration_and_dependency_edges() {
        use sea_orm::ConnectionTrait;
        let server = MockServer::start().await;
        let TestClient { client, _repos } =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        commit_pkgbuild(&repo, "initial", "1.0.0", &["mydep"]);
        let TwoProviders {
            parent, by_name, ..
        } = two_providers_of_one_name(&db, dir.path()).await;
        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(by_name),
            version_constraint: Set(String::new()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        let checkout = tempdir().unwrap();
        let services = Services::new(
            db.clone(),
            Arc::new(SnapshotStore::with_checkout_root(
                checkout.path().to_path_buf(),
            )),
            Arc::new(client),
            test_repo(),
            ActivityLog::discarding(),
        );
        let mut proposed = parent.clone();
        proposed.platforms = "x86_64;aarch64".parse().unwrap();
        proposed.patch = Some("invalid patch".into());
        assert!(
            super::edit_package(&services, &parent, &proposed)
                .await
                .is_err()
        );
        assert_eq!(
            Packages::find_by_id(parent.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
            parent
        );

        // Fail after the configuration write, while removing the old edges.
        proposed.patch = None;
        commit_pkgbuild(&repo, "remove dependency", "1.0.0", &[]);
        services.store.refresh(&parent.source_data).await.unwrap();
        db.execute_unprepared("CREATE TRIGGER fail_edge_delete BEFORE DELETE ON dependencies BEGIN SELECT RAISE(ABORT, 'injected edge failure'); END").await.unwrap();
        assert!(
            super::edit_package(&services, &parent, &proposed)
                .await
                .is_err()
        );
        assert_eq!(
            Packages::find_by_id(parent.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
            parent
        );
        assert_eq!(Dependencies::find().count(&db).await.unwrap(), 1);
        db.execute_unprepared("DROP TRIGGER fail_edge_delete")
            .await
            .unwrap();
        super::edit_package(&services, &parent, &proposed)
            .await
            .unwrap();
        assert_eq!(
            Packages::find_by_id(parent.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .platforms,
            proposed.platforms
        );
        assert_eq!(Dependencies::find().count(&db).await.unwrap(), 0);
    }
}
