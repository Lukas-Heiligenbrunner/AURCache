use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, QueueCause};
use aurcache_common::api::log::BuildRef;
use aurcache_common::build_state::BuildState;
use aurcache_db::dependencies;
use aurcache_db::helpers::build_enqueue::{
    Pending, Queued, enqueue_build_if_missing, promote_waiting_build,
};
use aurcache_db::helpers::builds::pending_build;
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use aurcache_db::{builds, packages};
use pacman_mirrors::platforms::Platform;
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QuerySelect,
};
use std::collections::{HashMap, HashSet};

/// Queue initial builds for a freshly-added set of packages.
///
/// - Packages with no AUR dependencies are enqueued as `ENQUEUED` and start immediately.
/// - Packages whose dependencies are already built get `ENQUEUED` too.
/// - Packages still waiting for one or more dependency builds receive a `WAITING_FOR_DEPS`
///   build record so that the UI can show them as "pending, waiting for deps".  They are
///   promoted to `ENQUEUED` automatically when the last blocking dependency finishes.
pub async fn trigger_initial_builds(
    db: &DatabaseConnection,
    activity: &ActivityLog,
    platforms: &[Platform],
    pkgbases: &[String],
) -> anyhow::Result<()> {
    if pkgbases.is_empty() {
        return Ok(());
    }
    // The rows, in one query — not one lookup per added package. Iteration
    // stays over `pkgbases` so a missing name is still skipped in order.
    let pkgs: HashMap<String, packages::Model> = Packages::find()
        .filter(packages::Column::Name.is_in(pkgbases.iter().map(String::as_str)))
        .all(db)
        .await?
        .into_iter()
        .map(|pkg| (pkg.name.clone(), pkg))
        .collect();
    // All names missed: the loop below would skip everything, and the empty
    // `IN` below is not valid SQL everywhere.
    if pkgs.is_empty() {
        return Ok(());
    }
    // Which of them have dependency edges, in one grouped query — zero-vs-one
    // is the only question, so the set of non-empty dependees is the answer.
    let has_deps: HashSet<i32> = Dependencies::find()
        .select_only()
        .column(dependencies::Column::DependentId)
        .filter(dependencies::Column::DependentId.is_in(pkgs.values().map(|pkg| pkg.id)))
        .group_by(dependencies::Column::DependentId)
        .into_tuple::<i32>()
        .all(db)
        .await?
        .into_iter()
        .collect();

    for pkgbase in pkgbases {
        let Some(pkg) = pkgs.get(pkgbase) else {
            continue;
        };
        // A leaf can start everywhere; anything else, wherever what it needs
        // is already built.
        let mut ready = vec![];
        let mut waiting = vec![];
        for platform in platforms {
            if !has_deps.contains(&pkg.id) || dependencies_satisfied(db, pkg.id, *platform).await? {
                ready.push(*platform);
            } else {
                waiting.push(*platform);
            }
        }
        for (platforms, pending) in [
            (ready, Pending::Enqueued),
            (waiting, Pending::WaitingForDeps),
        ] {
            if !platforms.is_empty() {
                trigger_build_for_package(db, activity, &platforms, pkg, pending).await?;
            }
        }
    }
    Ok(())
}

/// Startup scan: enqueue or promote builds for packages that have no pending or terminal build yet.
///
/// For each package × platform this function:
/// - Promotes any existing `WAITING_FOR_DEPS` build to `ENQUEUED` when all deps are satisfied.
/// - Inserts a new `ENQUEUED` build for packages with no existing build when deps are satisfied.
/// - Inserts a new `WAITING_FOR_DEPS` build for packages with no existing build when deps are not
///   yet satisfied, so they appear as pending in the UI.
///
/// Returns the number of builds newly promoted or inserted as `ENQUEUED` (i.e. startable).
pub async fn enqueue_missing_buildable_packages(
    db: &DatabaseConnection,
    activity: &ActivityLog,
) -> anyhow::Result<usize> {
    let packages = Packages::find().all(db).await?;

    let mut queued = 0;
    for pkg in packages {
        for &platform in pkg.platforms.as_slice() {
            let deps_ok = dependencies_satisfied(db, pkg.id, platform).await?;

            match pending_build(db, pkg.id, platform).await? {
                Some(b) if b.status == BuildState::WaitingForDeps => {
                    if deps_ok {
                        // All deps are now satisfied – promote it.
                        let Some(promoted) = promote_waiting_build(db, pkg.id, platform).await?
                        else {
                            continue;
                        };
                        activity.emit(Event::BuildUnblocked {
                            build: BuildRef {
                                pkgbase: pkg.name.clone(),
                                number: promoted.number,
                            },
                            by: None,
                        });
                        queued += 1;
                    }
                    // Deps still not satisfied – leave it waiting.
                }
                Some(_) => {
                    // ACTIVE or ENQUEUED build already present, nothing to do.
                }
                None => {
                    // No pending build.  Skip if there is any historical (terminal) build.
                    if build_exists_for_platform(db, pkg.id, platform).await? {
                        continue;
                    }
                    // Completely fresh: queue according to dep readiness.
                    let pending = if deps_ok {
                        Pending::Enqueued
                    } else {
                        Pending::WaitingForDeps
                    };
                    queued +=
                        trigger_build_for_package(db, activity, &[platform], &pkg, pending).await?;
                }
            }
        }
    }

    Ok(queued)
}

async fn build_exists_for_platform(
    db: &DatabaseConnection,
    pkg_id: i32,
    platform: Platform,
) -> anyhow::Result<bool> {
    Ok(Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform))
        .count(db)
        .await?
        != 0)
}

/// Whether everything `dependent_id` depends on is built, on `platform`.
async fn dependencies_satisfied(
    db: &DatabaseConnection,
    dependent_id: i32,
    platform: Platform,
) -> anyhow::Result<bool> {
    let deps = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent_id))
        .all(db)
        .await?;
    Ok(aurcache_db::helpers::builds::dependencies_satisfied(db, &deps, platform).await?)
}

/// Create or reuse a pending build entry for `pkg` on each of `platforms`.
///
/// Every build it inserts is logged as the package's first, whether it can
/// start or has to wait.
///
/// Returns the number of newly startable (`ENQUEUED`) builds that were inserted.
async fn trigger_build_for_package(
    db: &DatabaseConnection,
    activity: &ActivityLog,
    platforms: &[Platform],
    pkg: &packages::Model,
    pending: Pending,
) -> anyhow::Result<usize> {
    let version = pkg.upstream_version.clone().unwrap_or_default();
    let mut queued = 0;
    let mut inserted = vec![];

    for platform in platforms {
        let enqueue_result = enqueue_build_if_missing(
            db,
            pkg.id,
            *platform,
            &version,
            aurcache_db::helpers::time::now_secs(),
            pending,
            aurcache_common::build_state::BuildTrigger::User,
        )
        .await?;
        if enqueue_result.queued == Queued::Inserted {
            inserted.push(BuildRef {
                pkgbase: pkg.name.clone(),
                number: enqueue_result.build.number,
            });
            if pending == Pending::Enqueued {
                queued += 1;
            }
        }
    }

    if !inserted.is_empty() {
        activity.emit(Event::BuildQueued {
            pkg: pkg.name.as_str().into(),
            cause: QueueCause::First,
            builds: inserted,
            version: Some(version).filter(|version| !version.is_empty()),
            retried: None,
            needed_by: None,
        });
    }
    Ok(queued)
}
