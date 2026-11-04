//! A worker's heartbeat: the leases it renews, the builds it dropped, and the
//! configuration that rides along both ways.

use crate::worker_policy::worker_policy;
use crate::worker_settings::{record_effective, snapshot_for};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::worker::{Heartbeat, HeartbeatResponse};
use aurcache_db::helpers::{worker_jobs, worker_store};
use aurcache_db::workers;
use sea_orm::DatabaseConnection;

/// Take `heartbeat` from `worker`: renew the leases on the builds it lists,
/// abandon any it silently dropped, and answer the builds to stop and any
/// settings it does not hold yet.
///
/// The abort list rides this same answer rather than a second poll, so a
/// cancelled or lost build is stopped on the worker's next heartbeat.
pub async fn heartbeat(
    db: &DatabaseConnection,
    activity: &ActivityLog,
    worker: &mut workers::Model,
    heartbeat: &Heartbeat,
) -> anyhow::Result<HeartbeatResponse> {
    worker_store::touch_last_seen(db, worker, Some(&heartbeat.version)).await?;
    // Sent only when it changed, so this is normally absent.
    if let Some(effective) = &heartbeat.effective {
        record_effective(db, activity, worker, effective).await;
    }
    let outcome = worker_jobs::heartbeat(
        db,
        worker.id,
        &heartbeat.active_build_ids,
        worker_policy().lease_ttl_secs,
        worker_policy().max_attempts,
    )
    .await?;
    // Builds the worker stopped listing were abandoned above, each with a
    // fresh attempt while the budget allows. Said the way the reaper says it:
    // they were taken back from this worker.
    if !outcome.dropped.is_empty() {
        let named = crate::abandoned::named(&outcome.dropped);
        activity.emit(Event::WorkerReaped {
            workers: vec![worker.name.as_str().into()],
            retried: named.retried,
            failed: named.failed,
        });
        crate::abandoned::report(db, activity, &outcome.dropped).await;
    }
    // Read after the leases are renewed: see `snapshot_for`.
    Ok(HeartbeatResponse {
        cancel: outcome.cancel_requested,
        config: snapshot_for(db, worker, heartbeat.received_revision.as_deref()).await,
    })
}
