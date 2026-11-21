//! Signing what the repository published before it signed, in the background.
//!
//! Started when the server starts with a signing key, and again each time a
//! restore replaces that key, so that what the old key signed is signed with
//! the new one. Until a run finds nothing left to sign that a client can
//! install, the repository reports itself as backfilling, and a client
//! requiring signatures refuses whatever is still unsigned. A run that leaves
//! something unsigned is tried again later, less and less often, so a file
//! that cannot be signed yet neither stops the server nor floods the log.
//! Every run reports each listed file it cannot sign at all. One no client
//! can install either -- not on disk, or not the file its entry describes --
//! does not keep the backfill going; one that installs where signatures are
//! optional does, until its package is rebuilt.

use aurcache_activitylog::events::{Event, FileAction};
use aurcache_utils::repository::Repository;
use aurcache_utils::signing::{Backfill, Skipped, backfill};
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
        loop {
            sign_everything(&db, &repo).await;
            // `replace_key` has marked the repository unsigned already when
            // what signs changed; otherwise the run finds nothing to sign.
            repo.key_replaced().await;
        }
    });
}

/// What one run of the backfill comes to.
#[derive(Debug, PartialEq, Eq)]
enum Run {
    /// Nothing is left to sign with the key it ran with, which is still the
    /// repository's.
    Done,
    /// The key was replaced during the run, which is why whatever went wrong
    /// went wrong -- an update refuses what the old key signed -- and what it
    /// did sign, it signed with the old key: run again, with the new one, and
    /// report nothing.
    Restart,
    /// Something could not be signed, for this reason: tried again later.
    Retry { error: String },
}

/// What `outcome`, a run with the key the repository had when it started,
/// comes to, given whether that key was `replaced` since.
fn judge(outcome: &anyhow::Result<Backfill>, replaced: bool) -> Run {
    if replaced {
        return Run::Restart;
    }
    match outcome {
        Ok(Backfill::Complete { .. }) => Run::Done,
        Ok(Backfill::Incomplete {
            skipped, problems, ..
        }) => {
            let mut reasons = problems.clone();
            let installable = skipped.iter().filter(|skip| skip.why.installable()).count();
            if installable > 0 {
                reasons.push(format!(
                    "{installable} listed package file(s) cannot be signed until rebuilt, \
                     and clients that require signatures refuse them"
                ));
            }
            Run::Retry {
                error: reasons.join("; "),
            }
        }
        Err(e) => Run::Retry {
            error: format!("{e:#}"),
        },
    }
}

/// Run the backfill until a run finds nothing left to sign with the key the
/// repository has when it starts.
async fn sign_everything(db: &DatabaseConnection, repo: &Repository) {
    let mut wait = FIRST_WAIT;
    loop {
        let Some(key) = repo.key() else {
            return;
        };
        let outcome = backfill(db, repo).await;
        // Any swap counts, the same key read again included: what this run
        // signed, it signed with the key it started with.
        let replaced = repo
            .key()
            .is_some_and(|current| !Arc::ptr_eq(&current, &key));
        // Every run that stands reports what it signed and each file it
        // found it cannot sign; one run again with the new key reports
        // nothing.
        let error = match judge(&outcome, replaced) {
            Run::Done => {
                record(repo, &outcome);
                repo.mark_signed(&key);
                info!(
                    "Every package the repository publishes is signed with {}",
                    key.fingerprint()
                );
                return;
            }
            Run::Restart => {
                wait = FIRST_WAIT;
                continue;
            }
            Run::Retry { error } => {
                record(repo, &outcome);
                error
            }
        };
        repo.log().emit(Event::RepoSigningIncomplete { error });
        // A replaced key cuts the wait short: what it is to sign is all
        // there is to do, and waiting an hour for it would leave clients
        // that trust only the new key refusing what the old one signed.
        tokio::select! {
            () = tokio::time::sleep(wait) => wait = (wait * 2).min(LONGEST_WAIT),
            () = repo.key_replaced() => wait = FIRST_WAIT,
        }
    }
}

/// Record what a run signed, and each listed file it found it cannot sign.
fn record(repo: &Repository, outcome: &anyhow::Result<Backfill>) {
    let Ok(
        Backfill::Complete { signed, skipped }
        | Backfill::Incomplete {
            signed, skipped, ..
        },
    ) = outcome
    else {
        return;
    };
    if *signed > 0 {
        repo.log().emit(Event::RepoSigned { packages: *signed });
    }
    for Skipped { path, why } in skipped {
        repo.log().emit(Event::RepoFileFailed {
            path: path.display().to_string(),
            action: FileAction::Sign,
            error: why.explain(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_utils::repository::SignedWithReplacedKey;
    use aurcache_utils::signing::Unsignable;

    /// A run that failed because the key was replaced under it starts again
    /// at once, with nothing reported: the failure was the replacement.
    #[test]
    fn a_key_replaced_during_a_run_restarts_it_quietly() {
        let refused = anyhow::Error::new(SignedWithReplacedKey {
            filename: "hello-1.0-1-x86_64.pkg.tar.zst".to_string(),
        });
        assert_eq!(judge(&Err(refused), true), Run::Restart);
        assert_eq!(
            judge(
                &Ok(Backfill::Complete {
                    signed: 3,
                    skipped: Vec::new(),
                }),
                true
            ),
            Run::Restart,
            "what it signed, it signed with the old key"
        );
    }

    #[test]
    fn a_run_without_a_replacement_is_done_or_retried() {
        assert_eq!(
            judge(
                &Ok(Backfill::Complete {
                    signed: 0,
                    skipped: Vec::new(),
                }),
                false
            ),
            Run::Done
        );
        assert_eq!(
            judge(
                &Ok(Backfill::Incomplete {
                    signed: 1,
                    skipped: Vec::new(),
                    problems: vec!["x86_64/a: gone".to_string(), "x86_64/b: gone".to_string()],
                }),
                false
            ),
            Run::Retry {
                error: "x86_64/a: gone; x86_64/b: gone".to_string()
            }
        );
        assert!(matches!(
            judge(&Err(anyhow::anyhow!("the disk is full")), false),
            Run::Retry { error } if error == "the disk is full"
        ));
    }

    /// A run left incomplete only by a file clients still install but that
    /// cannot be signed says so, rather than retry with nothing to report.
    #[test]
    fn an_installable_file_left_unsigned_is_the_reason_given() {
        let Run::Retry { error } = judge(
            &Ok(Backfill::Incomplete {
                signed: 0,
                skipped: vec![Skipped {
                    path: "x86_64/hello-1.0-1-x86_64.pkg.tar.zst".into(),
                    why: Unsignable::NoChecksum,
                }],
                problems: Vec::new(),
            }),
            false,
        ) else {
            panic!("retried");
        };
        assert!(error.starts_with("1 listed package file(s)"), "{error}");
    }
}
