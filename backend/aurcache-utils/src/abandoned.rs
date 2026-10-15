//! Telling the logs about builds taken back from their workers.
//!
//! Four paths abandon builds -- the lease reaper, a heartbeat that stopped
//! listing one, a worker reporting one interrupted, and revoking a worker --
//! and all of them go through
//! `aurcache_db::helpers::worker_jobs::abandon_build`. What each build's log
//! and the activity log then say about it is the same whichever path it was,
//! so it is said here.

use crate::build_logger::append_build_output;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, QueueCause};
use aurcache_common::api::log::BuildRef;
use aurcache_db::helpers::worker_jobs::{Abandoned, Retry};
use sea_orm::DatabaseConnection;

/// The builds named as the activity log names them, split by whether they got
/// a fresh attempt. A build whose package is gone cannot be named, and is left
/// out.
pub struct Named {
    /// Handed back to the queue for another attempt.
    pub retried: Vec<BuildRef>,
    /// Out of attempts, and failed outright.
    pub failed: Vec<BuildRef>,
}

/// Name `abandoned` for an activity entry; see [`Named`].
#[must_use]
pub fn named(abandoned: &[Abandoned]) -> Named {
    let (failed, retried): (Vec<&Abandoned>, Vec<&Abandoned>) =
        abandoned.iter().partition(|a| a.retry == Retry::Spent);
    Named {
        retried: retried.iter().filter_map(|a| a.build_ref()).collect(),
        failed: failed.iter().filter_map(|a| a.build_ref()).collect(),
    }
}

/// Explain each abandoned build in its own log, and announce each fresh
/// attempt queued in its place.
///
/// Best-effort: the rows are already terminal and the retries already queued,
/// so nothing depends on this succeeding, and a failure to append is itself
/// logged.
pub async fn report(db: &DatabaseConnection, activity: &ActivityLog, abandoned: &[Abandoned]) {
    for build in abandoned {
        explain(activity, build).await;
        announce_retry(db, activity, build).await;
    }
}

/// Append why a build was abandoned to its log: the last line it gets.
async fn explain(activity: &ActivityLog, abandoned: &Abandoned) {
    let Some(build) = abandoned.build_ref() else {
        return;
    };
    let text = format!("Abandoned: {}.\n", abandoned.end_reason.explanation());
    if let Err(e) = append_build_output(&build.pkgbase, build.number, &text).await {
        activity.emit(Event::BuildLogAppendFailed {
            build,
            error: e.to_string(),
        });
    }
}

/// A replacement is a build queued like any other, and says which one it
/// repeats -- named as the operator knows it, not as the fresh row. An adopted
/// build was announced when it was queued, so only a fresh one is.
async fn announce_retry(db: &DatabaseConnection, activity: &ActivityLog, abandoned: &Abandoned) {
    let (Some(retried), Retry::Queued(replacement)) = (abandoned.build_ref(), abandoned.retry)
    else {
        return;
    };
    let builds = aurcache_db::helpers::builds::build_refs(db, &[replacement])
        .await
        .unwrap_or_default();
    activity.emit(Event::BuildQueued {
        pkg: retried.pkgbase.as_str().into(),
        cause: QueueCause::Retry,
        builds,
        version: None,
        retried: Some(retried),
        needed_by: None,
    });
}
