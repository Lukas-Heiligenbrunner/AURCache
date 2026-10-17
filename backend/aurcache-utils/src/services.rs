use aurcache_activitylog::activity_utils::ActivityLog;
use std::sync::Arc;

use aurcache_deps::AurClient;
use sea_orm::DatabaseConnection;

use crate::repository::Repository;
use crate::snapshot::SnapshotStore;

/// What a package operation acts through: the database it writes, the source cache it resolves through, the AUR
/// client it resolves dependencies with, the repository it publishes to and
/// the log it records to.
///
/// This is where those live, rather than values threaded separately from
/// `main` and reassembled at every layer. A function that needs more than one
/// of them takes this; a function that needs exactly one takes that one, so its
/// signature still says what it touches.
///
/// Cloning is a few refcount bumps -- every member is a handle or behind an
/// `Arc` -- so a spawned task takes a clone rather than borrowing, and no
/// separate owned form is needed.
#[derive(Clone)]
pub struct Services {
    pub db: DatabaseConnection,
    /// Resolves and caches package sources.
    pub store: Arc<SnapshotStore>,
    /// Resolves dependency names against the official repositories and the AUR.
    pub client: Arc<AurClient>,
    /// The pacman repository, which every change to goes through.
    pub repo: Arc<Repository>,
    /// Where anything worth a line in the log is recorded. A handle, not a
    /// connection: one task owns the writing, so nothing here has to decide
    /// what to do when a write fails.
    pub activity: ActivityLog,
}

impl Services {
    #[must_use]
    pub fn new(
        db: DatabaseConnection,
        store: Arc<SnapshotStore>,
        client: Arc<AurClient>,
        repo: Arc<Repository>,
        activity: ActivityLog,
    ) -> Self {
        Self {
            db,
            store,
            client,
            repo,
            activity,
        }
    }
}
