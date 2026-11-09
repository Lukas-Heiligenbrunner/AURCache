//! Lease reaper: a periodic scheduler job that reclaims build jobs whose remote
//! worker went silent, and backstops builds that have run implausibly long.
//!
//! Liveness model (see design doc §"Build liveness & failure handling"): each
//! `ACTIVE` build carries a `lease_expires_at` that a worker renews via
//! heartbeats. When a worker crashes/partitions, the lease lapses and this job
//! abandons the build: the row is written terminal `FAILED` (naming the worker
//! that ran it) and a *fresh* build is queued while the derived retry budget
//! allows — never a requeue of the same row. Explicit failures are handled
//! synchronously in the `complete` endpoint and are never seen here.

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::api::log::WorkerRef;
use aurcache_common::build_state::BuildState;
use aurcache_db::helpers::time::now_secs;
use aurcache_db::helpers::worker_jobs::{Abandoned, BuildAges, reap_expired_builds};
use aurcache_db::prelude::Workers;
use aurcache_db::{builds, workers};
use aurcache_utils::settings;
use sea_orm::DatabaseConnection;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Spawn the reaper loop. Runs every `REAP_INTERVAL` seconds (default 20s).
pub fn start_lease_reaper(db: DatabaseConnection, activity: ActivityLog) -> JoinHandle<()> {
    let policy = aurcache_utils::worker_policy::worker_policy();
    let interval = Duration::from_secs(policy.reap_interval_secs);
    let max_attempts = policy.max_attempts;
    // Backstop grace beyond a build's own timeout before we forcibly reclaim a
    // still-heartbeating but hung build.
    let grace = policy.reap_backstop_grace_secs;

    tokio::spawn(async move {
        info!(
            "Lease reaper started (every {}s, max_attempts={max_attempts})",
            interval.as_secs()
        );
        loop {
            tokio::time::sleep(interval).await;

            let ages = match build_ages(&db, grace).await {
                Ok(ages) => ages,
                Err(error) => {
                    tracing::warn!("could not read job timeout: {error}");
                    continue;
                }
            };

            match reap_expired_builds(&db, now_secs(), max_attempts, &ages).await {
                Ok(abandoned) => {
                    if !abandoned.is_empty() {
                        // One entry for the pass, not one per build: the reaper
                        // finds them together and they have one cause. Named
                        // as the operator knows them -- the build they were
                        // watching, not the fresh row standing in for it.
                        let named = aurcache_utils::abandoned::named(&abandoned);
                        warn!(
                            "Lease reaper: retried {:?}, gave up on {:?} (silent/hung workers)",
                            named.retried, named.failed
                        );
                        activity.emit(Event::WorkerReaped {
                            workers: worker_names(&db, &abandoned).await,
                            retried: named.retried,
                            failed: named.failed,
                        });
                    }
                    aurcache_utils::abandoned::report(&db, &activity, &abandoned).await;
                }
                Err(e) => warn!("Lease reaper pass failed: {e}"),
            }
        }
    })
}

/// How long each running build may take: its package's job timeout -- the
/// server's, unless the package sets its own -- plus `grace`.
async fn build_ages(db: &DatabaseConnection, grace: i64) -> anyhow::Result<BuildAges> {
    let age = |timeout: u64| {
        i64::try_from(timeout)
            .unwrap_or(i64::MAX)
            .saturating_add(grace)
    };
    let server = settings::get(db, settings::key::JOB_TIMEOUT, None).await?;
    let mut ages = BuildAges::all(age(server.value.0));
    let running: Vec<i32> = aurcache_db::prelude::Builds::find()
        .select_only()
        .column(builds::Column::PkgId)
        .filter(builds::Column::Status.eq(BuildState::Active))
        .distinct()
        .into_tuple()
        .all(db)
        .await?;
    for pkg_id in running {
        let timeout = settings::get(db, settings::key::JOB_TIMEOUT, Some(pkg_id)).await?;
        ages.by_package.insert(pkg_id, age(timeout.value.0));
    }
    Ok(ages)
}

/// The names of the workers that held these builds, for the entry that says
/// they went silent. Best-effort: a worker row that cannot be read is left out
/// rather than holding the entry back.
async fn worker_names(db: &DatabaseConnection, abandoned: &[Abandoned]) -> Vec<WorkerRef> {
    let mut ids: Vec<i32> = abandoned.iter().filter_map(|a| a.worker_id).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Vec::new();
    }
    Workers::find()
        .select_only()
        .column(workers::Column::Name)
        .filter(workers::Column::Id.is_in(ids))
        .into_tuple::<String>()
        .all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(WorkerRef::from)
        .collect()
}
