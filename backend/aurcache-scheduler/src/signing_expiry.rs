//! Warning that the repository's signing key stops signing soon.
//!
//! A server that starts with a key within
//! [`aurcache_signing::EXPIRY_WARNING_DAYS`] of it says so at once; one that
//! starts earlier says so once it is, if it is still running then.

use aurcache_activitylog::events::Event;
use aurcache_signing::Expiring;
use aurcache_utils::repository::Repository;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

/// The longest the warning sleeps before looking at the clock again, so a
/// clock that jumped, or a host that was suspended, is followed rather than
/// slept through.
const LONGEST_SLEEP: Duration = Duration::from_secs(24 * 60 * 60);

/// Record that the repository's key stops signing soon, now or once it does.
pub fn start_expiry_warning(repo: Arc<Repository>) {
    let Some(key) = repo.key() else {
        return;
    };
    if let Some(expiring) = key.expiring() {
        // Loading the key logged it already.
        repo.log().emit(event(expiring));
        return;
    }
    if key.warns_in().is_none() {
        return;
    }
    tokio::spawn(async move {
        let Some(key) = repo.key() else {
            return;
        };
        while let Some(wait) = key.warns_in() {
            tokio::time::sleep(wait.min(LONGEST_SLEEP)).await;
        }
        if let Some(expiring) = key.expiring() {
            warn!("{expiring}");
            repo.log().emit(event(expiring));
        }
    });
}

fn event(expiring: Expiring) -> Event {
    let Expiring {
        signer,
        on,
        primary,
    } = expiring;
    Event::RepoKeyExpiring {
        signer,
        on,
        primary,
    }
}
