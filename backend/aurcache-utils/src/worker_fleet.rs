//! The fleet as an operator sees it.

use crate::worker_policy::worker_policy;
use crate::worker_settings::rejected_count;
use aurcache_common::api::worker::WorkerSummary;
use aurcache_db::helpers::time::now_secs;
use aurcache_db::helpers::worker_jobs;
use aurcache_db::helpers::worker_store::{self, BuildTally};
use aurcache_db::workers;
use sea_orm::{DatabaseConnection, DbErr};

/// Every worker, with what it is running and how its builds went.
pub async fn list(db: &DatabaseConnection) -> Result<Vec<WorkerSummary>, DbErr> {
    // Independent reads: serial awaits would pay both round trips in turn.
    let (workers, tallies) = tokio::try_join!(
        worker_store::list_workers(db),
        worker_store::build_tallies(db),
    )?;
    let now = now_secs();
    let timeout = worker_policy().liveness_timeout_secs;
    Ok(workers
        .into_iter()
        .map(|worker| {
            let tally = tallies.get(&worker.id).copied().unwrap_or_default();
            summarise(worker, tally, now, timeout)
        })
        .collect())
}

/// A worker row as the operator's view of it.
///
/// Not the row itself: that carries `signed_cert`, and there is no reason to
/// hand a browser the certificate issued to a build machine.
fn summarise(worker: workers::Model, tally: BuildTally, now: i64, timeout: i64) -> WorkerSummary {
    WorkerSummary {
        settings_rejected: rejected_count(&worker),
        id: worker.id,
        name: worker.name,
        status: worker.status,
        cert_fingerprint: worker.cert_fingerprint,
        arches: worker.arches.into_vec(),
        package_affinity: worker.package_affinity.into_vec(),
        priority: worker.priority,
        last_seen: worker.last_seen,
        version: worker.version,
        kind: worker.kind,
        online: worker_jobs::is_live(worker.last_seen, now, timeout),
        active_builds: tally.active,
        successful_builds: tally.successful,
        failed_builds: tally.failed,
        paused: worker.paused,
    }
}
