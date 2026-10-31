pub mod activity_retention;
pub mod auto_update;
pub mod lease_reaper;
pub mod mirror_ranking;
pub mod official_repos;
pub mod retired_packages;
pub mod update_version_check;

use aurcache_common::schedule::Schedule;
use aurcache_common::units::parse_duration;
use std::time::Duration;
use tracing::{debug, info, warn};

/// A duration from the environment variable `key`, as [`parse_duration`] reads
/// one (`90d`, `12h`, or plain seconds); `default` when it is unset.
///
/// A value that does not parse is the default too, with a warning: `90d` read
/// as a plain number silently meant the default, which is how a typo goes
/// unnoticed.
pub(crate) fn env_duration(key: &str, default: Duration) -> Duration {
    let Some(raw) = std::env::var(key).ok().filter(|v| !v.trim().is_empty()) else {
        return default;
    };
    parse_duration(&raw).map_or_else(
        || {
            warn!(
                "ignoring {key}={raw:?} (expected seconds, or a duration such as 12h or 90d); \
                 using {}s",
                default.as_secs()
            );
            default
        },
        Duration::from_secs,
    )
}

/// Sleep until the schedule's next run, or for `at_most` if that comes first.
///
/// Schedules are read in the server's local timezone (`TZ`, else
/// `/etc/localtime`), so `0 3 * * *` means 3 am where the server is, which
/// is what someone writing it expects. The settings page says which timezone
/// that is.
///
/// The next run is worked out from the clock each time rather than carried
/// over, so a clock that jumped is followed rather than slept through. A job
/// whose schedule can change while it waits passes a bound, and reads the
/// schedule again on [`Wake::Recheck`]: sleeping until the old one's next run
/// would leave a weekly schedule edited to hourly waiting out the week.
pub(crate) async fn sleep_until_next_fire(
    schedule: &Schedule,
    what: &str,
    at_most: Duration,
) -> Wake {
    let now = jiff::Zoned::now();
    let Some(next) = schedule.next_after(&now) else {
        return Wake::Exhausted;
    };
    let duration = Duration::try_from(now.duration_until(&next)).unwrap_or(Duration::ZERO);
    if duration > at_most {
        debug!("Next scheduled {what} at {next}; checking the schedule again first");
        tokio::time::sleep(at_most).await;
        return Wake::Recheck;
    }
    info!(
        "Waiting for scheduled {what} until {next} ({} seconds)",
        duration.as_secs()
    );
    tokio::time::sleep(duration).await;
    Wake::Fired
}

/// Why [`sleep_until_next_fire`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// The schedule's next occurrence is now: run the job.
    Fired,
    /// The bound passed first: read the schedule again.
    Recheck,
    /// The schedule has no upcoming occurrence, so there was nothing to sleep
    /// until.
    Exhausted,
}
