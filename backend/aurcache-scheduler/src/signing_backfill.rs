//! Signing what the repository published before it signed, in the background.
//!
//! Started once, when the server starts with a signing key. Until a run finds
//! nothing left to sign, the repository reports itself as backfilling, and a
//! client requiring signatures refuses whatever is still unsigned. A run that
//! leaves something unsigned is tried again later, less and less often, so a
//! file that cannot be signed neither stops the server nor floods the log.

use aurcache_activitylog::events::Event;
use aurcache_utils::repository::Repository;
use aurcache_utils::signing::{Backfill, backfill};
use sea_orm::DatabaseConnection;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// How long after an incomplete run the next one starts. Doubles after each
/// one, up to [`LONGEST_WAIT`].
const FIRST_WAIT: Duration = Duration::from_secs(60);
const LONGEST_WAIT: Duration = Duration::from_secs(60 * 60);

/// Spawn the backfill, when the repository signs.
pub fn start_signing_backfill(db: DatabaseConnection, repo: Arc<Repository>) {
    if repo.key().is_none() {
        return;
    }
    tokio::spawn(async move {
        let mut wait = FIRST_WAIT;
        loop {
            let outcome = backfill(&db, &repo).await;
            let signed = match &outcome {
                Ok(Backfill::Complete { signed } | Backfill::Incomplete { signed, .. }) => *signed,
                Err(_) => 0,
            };
            if signed > 0 {
                repo.log().emit(Event::RepoSigned { packages: signed });
            }
            let error = match outcome {
                Ok(Backfill::Complete { .. }) => {
                    repo.mark_signed();
                    info!("Every package the repository publishes is signed");
                    return;
                }
                Ok(Backfill::Incomplete { problems, .. }) => problems.join("; "),
                Err(e) => format!("{e:#}"),
            };
            repo.log().emit(Event::RepoSigningIncomplete { error });
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(LONGEST_WAIT);
        }
    });
}
