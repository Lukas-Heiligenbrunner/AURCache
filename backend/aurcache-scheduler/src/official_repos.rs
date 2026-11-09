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

/// What the refresh last reported, so it reports only a change: it runs every
/// minute and has nothing new to say on almost all of them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Nothing reported yet.
    Start,
    Current,
    Failing,
}

/// Keep the official repository databases, and the names read from them,
/// current for as long as the server runs.
///
/// Said once when they are current, once per outage, and once again when it
/// recovers.
pub fn start_official_repo_refresh(
    client: Arc<AurClient>,
    activity: ActivityLog,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = State::Start;
        loop {
            let next = match client.official.refresh().await {
                Ok(()) => State::Current,
                Err(e) => {
                    if state != State::Failing {
                        activity.emit(Event::OfficialReposRefreshFailed {
                            error: e.to_string(),
                        });
                    }
                    State::Failing
                }
            };
            if next == State::Current && state != State::Current {
                info!("official repository databases are current");
            }
            state = next;
            tokio::time::sleep(TICK).await;
        }
    })
}
