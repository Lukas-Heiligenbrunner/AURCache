//! Signing what the repository published before it signed, in the background.
//!
//! Started when the server starts with a signing key, and again whenever the
//! component key that signs changes -- a subkey a day from expiring giving way
//! to the next -- so that what the old one signed is signed with the new one
//! before clients refuse it. Until a run finds nothing left to sign, the
//! repository reports itself as backfilling, and a client requiring
//! signatures refuses whatever is still unsigned. A run that leaves something
//! unsigned is tried again later, less and less often, so a file that cannot
//! be signed neither stops the server nor floods the log. While the key that
//! signs is close to expiring, the job says so once a day.

use aurcache_activitylog::events::Event;
use aurcache_utils::repository::Repository;
use aurcache_utils::signing::{Backfill, backfill};
use sea_orm::DatabaseConnection;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// How long after an incomplete run the next one starts. Doubles after each
/// one, up to [`LONGEST_WAIT`].
const FIRST_WAIT: Duration = Duration::from_secs(60);
const LONGEST_WAIT: Duration = Duration::from_secs(60 * 60);

/// How often the job looks again at a key close to expiring, and says so.
const NOTICE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Spawn the backfill, when the repository signs.
pub fn start_signing_backfill(db: DatabaseConnection, repo: Arc<Repository>) {
    if repo.key().is_none() {
        return;
    }
    tokio::spawn(async move {
        loop {
            sign_everything(&db, &repo).await;
            signer_changes(&repo).await;
            // What the old component key signed is not signed by the new one.
            repo.mark_unsigned();
        }
    });
}

/// Run the backfill until a run finds nothing left to sign.
async fn sign_everything(db: &DatabaseConnection, repo: &Repository) {
    let mut wait = FIRST_WAIT;
    loop {
        let outcome = backfill(db, repo).await;
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
}

/// Wait until the component key that signs changes ([`InstanceKey::signer_changes_in`]),
/// repeating the notice of its expiry once a day meanwhile. Never returns
/// for a key that does not expire.
///
/// [`InstanceKey::signer_changes_in`]: aurcache_signing::InstanceKey::signer_changes_in
async fn signer_changes(repo: &Repository) {
    let Some(key) = repo.key() else {
        return std::future::pending().await;
    };
    let signer = key.signer_fingerprint();
    loop {
        if let Some(notice) = key.expiry_notice() {
            warn!("{notice}");
        }
        let Some(change) = key.signer_changes_in() else {
            return std::future::pending().await;
        };
        tokio::time::sleep(change.min(NOTICE_INTERVAL)).await;
        if key.signer_fingerprint() != signer {
            return;
        }
    }
}
