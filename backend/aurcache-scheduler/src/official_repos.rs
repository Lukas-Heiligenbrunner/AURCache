use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use std::sync::Arc;
use std::time::Duration;

use aurcache_deps::AurClient;
use tokio::task::JoinHandle;
use tracing::info;

/// How often to ask whether the official repository databases need anything.
///
/// Not the same as how often they are downloaded: a tick with nothing stale
/// costs three `stat`s and returns, and the databases' own hour-long TTL is
/// what decides when one is actually fetched. The short tick is how quickly a
/// failed refresh is retried -- and on a fresh instance, how quickly the first
/// one lands, since the server serves while this runs and dependency
/// resolution cannot answer until it has.
const TICK: Duration = Duration::from_secs(60);

/// Keep the official repository databases, and the names read from them,
/// current for as long as the server runs.
pub fn start_official_repo_refresh(
    client: Arc<AurClient>,
    activity: ActivityLog,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut first = true;
        let mut failing = false;
        loop {
            match client.official.refresh().await {
                // Only worth saying once per outage, and once when it
                // recovers: this runs every minute and has nothing to report
                // on almost all of them.
                Ok(()) => {
                    failing = false;
                    if first {
                        info!("official repository databases are current");
                        first = false;
                    }
                }
                Err(e) => {
                    // Recorded once per outage: it is retried every minute.
                    if !failing {
                        activity.emit(Event::OfficialReposRefreshFailed {
                            error: e.to_string(),
                        });
                        failing = true;
                    }
                    first = true;
                }
            }
            tokio::time::sleep(TICK).await;
        }
    })
}
