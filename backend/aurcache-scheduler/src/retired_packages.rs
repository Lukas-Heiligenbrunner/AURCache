//! Deleting package files the repository stopped listing, once no client can
//! still be about to download them.
//!
//! Publishing a new version takes the old one out of `repo.db` straight away
//! but leaves its file in place: a client that ran `pacman -Sy` just before
//! still has the old `repo.db`, and asks for the old file. This job deletes
//! such files once they have been unlisted for `RETIRED_PACKAGE_GRACE` (see
//! [`Repository::sweep`]).

use aurcache_activitylog::events::Event;
use aurcache_utils::repository::Repository;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::info;

/// How long a retired package file stays downloadable, by default: a day,
/// comfortably longer than any upgrade takes between syncing and downloading.
const DEFAULT_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// The longest the job sleeps between sweeps. A file is deleted at most this
/// long after its grace runs out.
const MAX_INTERVAL: Duration = Duration::from_secs(60 * 60);

fn grace() -> Duration {
    crate::env_duration("RETIRED_PACKAGE_GRACE", DEFAULT_GRACE)
}

/// Sweep often enough that a short grace is honoured roughly, without
/// reading every platform directory more than once a minute.
fn interval(grace: Duration) -> Duration {
    (grace / 2).clamp(Duration::from_secs(60), MAX_INTERVAL)
}

/// Spawn the sweep loop.
pub fn start_retired_package_sweep(
    db: sea_orm::DatabaseConnection,
    repo: Arc<Repository>,
) -> JoinHandle<()> {
    let grace = grace();
    let interval = interval(grace);
    tokio::spawn(async move {
        info!(
            "Retired package sweep started (grace {}s, every {}s)",
            grace.as_secs(),
            interval.as_secs()
        );
        loop {
            tokio::time::sleep(interval).await;
            match repo.sweep(&db, grace).await {
                Ok(0) => {}
                // What went is recorded by the sweep itself, by name.
                Ok(deleted) => info!("deleted {deleted} retired package file(s)"),
                Err(e) => repo.log().emit(Event::RepoSweepFailed {
                    error: format!("{e:#}"),
                }),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interval_follows_the_grace_within_bounds() {
        assert_eq!(interval(Duration::from_secs(10)), Duration::from_secs(60));
        assert_eq!(interval(Duration::from_secs(600)), Duration::from_secs(300));
        assert_eq!(interval(DEFAULT_GRACE), MAX_INTERVAL);
    }
}
