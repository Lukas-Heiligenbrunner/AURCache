use anyhow::anyhow;
use aurcache_db::dependencies;
use aurcache_db::helpers::build_enqueue::{enqueue_build_if_missing, promote_waiting_build};
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use aurcache_db::{builds, packages};
use aurcache_types::builder::{Action, BuildStates};
use futures::future::try_join_all;
use pacman_mirrors::platforms::Platform;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};

use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;
use tracing::warn;

/// Queue initial builds for a freshly-added set of packages.
///
/// - Packages with no AUR dependencies are enqueued as `ENQUEUED` and start immediately.
/// - Packages whose dependencies are already built get `ENQUEUED` too.
/// - Packages still waiting for one or more dependency builds receive a `WAITING_FOR_DEPS`
///   build record so that the UI can show them as "pending, waiting for deps".  They are
///   promoted to `ENQUEUED` automatically when the last blocking dependency finishes.
pub async fn trigger_initial_builds(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: &[Platform],
    pkgbases: &[String],
) -> anyhow::Result<()> {
    for pkgbase in pkgbases {
        let Some(pkg) = Packages::find()
            .filter(packages::Column::Name.eq(pkgbase))
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
            // Leaf package – no deps, can start right away.
            trigger_build_for_package(db, tx, platforms, pkg, BuildStates::ENQUEUED_BUILD).await?;
        } else {
            // Has AUR dependencies.  Check per-platform whether they are already satisfied.
            let platform_readiness = try_join_all(platforms.iter().map(|platform| async move {
                let ready = dependencies_satisfied(db, pkg.id, platform).await?;
                Ok::<_, anyhow::Error>((*platform, ready))
            }))
            .await?;

            let mut ready_platforms = vec![];
            let mut waiting_platforms = vec![];
            for (platform, ready) in platform_readiness {
                if ready {
                    ready_platforms.push(platform);
                } else {
                    waiting_platforms.push(platform);
                }
            }

            if !ready_platforms.is_empty() {
                trigger_build_for_package(
                    db,
                    tx,
                    &ready_platforms,
                    pkg.clone(),
                    BuildStates::ENQUEUED_BUILD,
                )
                .await?;
            }
            if !waiting_platforms.is_empty() {
                trigger_build_for_package(
                    db,
                    tx,
                    &waiting_platforms,
                    pkg,
                    BuildStates::WAITING_FOR_DEPS,
                )
                .await?;
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
    tx: &Sender<Action>,
) -> anyhow::Result<usize> {
    let packages = Packages::find().all(db).await?;

    let mut queued = 0;
    for pkg in packages {
        let platforms = match parse_platforms(&pkg.platforms) {
            Ok(platforms) => platforms,
            Err(error) => {
                warn!(
                    "Skipping package {} during startup enqueue because platforms are invalid: {error}",
                    pkg.name
                );
                continue;
            }
        };

        for platform in platforms {
            let deps_ok = dependencies_satisfied(db, pkg.id, &platform).await?;

            match pending_build_for_platform(db, pkg.id, &platform).await? {
                Some(b) if b.status == Some(BuildStates::WAITING_FOR_DEPS) => {
                    if deps_ok {
                        // All deps are now satisfied – promote and dispatch.
                        let txn = db.begin().await?;
                        let Some(promoted) = promote_waiting_build(&txn, pkg.id, platform).await?
                        else {
                            txn.commit().await?;
                            continue;
                        };
                        // Reflect the promotion in the package's own status.
                        let mut pkg_active: packages::ActiveModel = pkg.clone().into();
                        pkg_active.status = Set(BuildStates::ENQUEUED_BUILD);
                        pkg_active.save(&txn).await?;
                        txn.commit().await?;
                        let _ = tx.send(Action::Build(Box::from(pkg.clone()), Box::new(promoted)));
                        queued += 1;
                    }
                    // Deps still not satisfied – leave it waiting.
                }
                Some(_) => {
                    // ACTIVE or ENQUEUED build already present, nothing to do.
                }
                None => {
                    // No pending build.  Skip if there is any historical (terminal) build.
                    if build_exists_for_platform(db, pkg.id, &platform).await? {
                        continue;
                    }
                    // Completely fresh: queue according to dep readiness.
                    if deps_ok {
                        queued += trigger_build_for_package(
                            db,
                            tx,
                            &[platform],
                            pkg.clone(),
                            BuildStates::ENQUEUED_BUILD,
                        )
                        .await?;
                    } else {
                        trigger_build_for_package(
                            db,
                            tx,
                            &[platform],
                            pkg.clone(),
                            BuildStates::WAITING_FOR_DEPS,
                        )
                        .await?;
                    }
                }
            }
        }
    }

    Ok(queued)
}

/// Return the single pending build (ACTIVE / ENQUEUED / WAITING_FOR_DEPS) for a package on a
/// platform, or `None` if no pending build exists.
async fn pending_build_for_platform(
    db: &DatabaseConnection,
    pkg_id: i32,
    platform: &Platform,
) -> anyhow::Result<Option<builds::Model>> {
    Ok(Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform.as_str()))
        .filter(builds::Column::Status.is_in(vec![
            Some(BuildStates::ACTIVE_BUILD),
            Some(BuildStates::ENQUEUED_BUILD),
            Some(BuildStates::WAITING_FOR_DEPS),
        ]))
        .one(db)
        .await?)
}

async fn build_exists_for_platform(
    db: &DatabaseConnection,
    pkg_id: i32,
    platform: &Platform,
) -> anyhow::Result<bool> {
    Ok(Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform.as_str()))
        .count(db)
        .await?
        != 0)
}

async fn dependencies_satisfied(
    db: &DatabaseConnection,
    dependent_id: i32,
    platform: &Platform,
) -> anyhow::Result<bool> {
    let deps = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent_id))
        .all(db)
        .await?;

    for dep in deps {
        let Some((version,)) = Builds::find()
            .select_only()
            .column(builds::Column::Version)
            .filter(builds::Column::PkgId.eq(dep.dependee_id))
            .filter(builds::Column::Platform.eq(platform.as_str()))
            .filter(builds::Column::Status.eq(Some(BuildStates::SUCCESSFUL_BUILD)))
            .order_by_desc(builds::Column::EndTime)
            .into_tuple::<(String,)>()
            .one(db)
            .await?
        else {
            return Ok(false);
        };

        if !crate::pkg::satisfies_constraint(&version, &dep.version_constraint) {
            return Ok(false);
        }
    }

    Ok(true)
}

/// Create or reuse a pending build entry for `pkg` on each of `platforms`.
///
/// The build is inserted with `initial_status` (either `ENQUEUED_BUILD` or `WAITING_FOR_DEPS`).
/// `Action::Build` is only dispatched for `ENQUEUED_BUILD` builds, because `WAITING_FOR_DEPS`
/// builds must not be started until their dependencies are ready.
///
/// Returns the number of newly startable (`ENQUEUED`) builds that were inserted.
async fn trigger_build_for_package(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: &[Platform],
    pkg: packages::Model,
    initial_status: i32,
) -> anyhow::Result<usize> {
    let version = pkg.upstream_version.clone().unwrap_or_default();
    let mut queued = 0;

    for platform in platforms {
        let txn = db.begin().await?;
        let enqueue_result = enqueue_build_if_missing(
            &txn,
            pkg.id,
            *platform,
            &version,
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
            initial_status,
        )
        .await?;

        if enqueue_result.inserted {
            let mut pkg_active: packages::ActiveModel = pkg.clone().into();
            pkg_active.latest_build = Set(Some(enqueue_result.build.id));
            pkg_active.status = Set(initial_status);
            pkg_active.save(&txn).await?;
        }

        txn.commit().await?;
        if enqueue_result.inserted && initial_status == BuildStates::ENQUEUED_BUILD {
            let _ = tx.send(Action::Build(
                Box::from(pkg.clone()),
                Box::from(enqueue_result.build),
            ));
            queued += 1;
        }
    }

    Ok(queued)
}

fn parse_platforms(platforms: &str) -> anyhow::Result<Vec<Platform>> {
    Platform::parse_many(platforms)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("Invalid platforms '{platforms}' for queued dependency build: {e}"))
}
