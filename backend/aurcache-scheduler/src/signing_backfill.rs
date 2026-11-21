//! Signing what the repository published before it signed, in the background.
//!
//! Started when the server starts with a signing key. Until a run finds
//! nothing left to sign, the repository reports itself as backfilling, and a
//! client requiring signatures refuses whatever is still unsigned. A run that
//! leaves something unsigned is tried again later, less and less often, so a
//! file that cannot be signed yet neither stops the server nor floods the
//! log. A listed file that can never be signed -- not on disk, or not the
//! package its entry describes -- is reported once, and does not keep the
//! backfill going.

use aurcache_activitylog::events::{Event, FileAction};
use aurcache_utils::repository::Repository;
use aurcache_utils::signing::{Backfill, Backfiller, Skipped};
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
        sign_everything(&db, &repo, &mut Backfiller::default()).await;
    });
}

/// Run the backfill until a run finds nothing left to sign.
async fn sign_everything(db: &DatabaseConnection, repo: &Repository, backfiller: &mut Backfiller) {
    let mut wait = FIRST_WAIT;
    loop {
        let error = match backfiller.run(db, repo).await {
            Ok(Backfill::Complete { signed, skipped }) => {
                record(repo, signed, skipped);
                repo.mark_signed();
                info!("Every package the repository publishes is signed");
                return;
            }
            Ok(Backfill::Incomplete {
                signed,
                skipped,
                problems,
            }) => {
                record(repo, signed, skipped);
                problems.join("; ")
            }
            Err(e) => format!("{e:#}"),
        };
        repo.log().emit(Event::RepoSigningIncomplete { error });
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(LONGEST_WAIT);
    }
}

/// Record what a run signed, and each listed file it found it cannot sign.
fn record(repo: &Repository, signed: usize, skipped: Vec<Skipped>) {
    if signed > 0 {
        repo.log().emit(Event::RepoSigned { packages: signed });
    }
    for Skipped { path, why } in skipped {
        repo.log().emit(Event::RepoFileFailed {
            path: path.display().to_string(),
            action: FileAction::Sign,
            error: why.explain(),
        });
    }
}
