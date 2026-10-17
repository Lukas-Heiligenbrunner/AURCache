//! Keeping the log from growing for ever.
//!
//! Nothing has ever deleted an entry, and the log only grows -- faster now that
//! the server records its own restarts and the failures it notices. Every
//! listing reads it, counts it and pages it, so an unbounded table is a page
//! that gets slower for as long as the instance runs.
//!
//! Age rather than row count, because that is how people talk about a log: "the
//! last three months", not "the last fifty thousand things". Set
//! `ACTIVITY_RETENTION=0` for a deployment that would rather the log were
//! complete than bounded. (Named for the activity log the structured log
//! replaced, so existing deployments keep their setting.)

use aurcache_activitylog::log_store::LogStore;
use aurcache_db::helpers::time::now_secs;
use sea_orm::DatabaseConnection;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// How long an entry is kept by default: long enough that "what happened around
/// the time that package broke" is still answerable a season later.
const DEFAULT_RETENTION: Duration = Duration::from_secs(90 * 24 * 60 * 60);

/// How often to sweep. A log entry is not urgent to delete, and reading the
/// whole table more than once an hour would cost more than it saves.
const INTERVAL: Duration = Duration::from_secs(60 * 60);

fn retention_secs() -> u64 {
    crate::env_duration("ACTIVITY_RETENTION", DEFAULT_RETENTION).as_secs()
}

/// Spawn the sweep loop.
#[must_use]
pub fn start_activity_retention(db: DatabaseConnection) -> JoinHandle<()> {
    tokio::spawn(async move {
        let keep = retention_secs();
        if keep == 0 {
            info!("Log retention disabled; entries are kept for ever");
            // Parked rather than returned: the server runs for as long as every
            // one of its tasks does, and this one finishing would stop it.
            return std::future::pending().await;
        }
        info!("Log retention: keeping {keep}s of entries");

        let log = LogStore::new(db);
        loop {
            // Swept before the first sleep as well, so an instance that is
            // restarted more often than the interval still prunes.
            match log.prune(keep, now_secs()).await {
                Ok(0) => {}
                Ok(deleted) => info!("Pruned {deleted} log entries"),
                Err(e) => warn!("Log retention pass failed: {e}"),
            }
            tokio::time::sleep(INTERVAL).await;
        }
    })
}
