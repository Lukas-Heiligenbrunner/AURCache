use aurcache_activitylog::activity_utils::ActivityLog;
use std::env;
use std::path::{Path, PathBuf};

use aurcache_activitylog::events::Event;
use aurcache_common::api::worker::ApprovalStatus;
use aurcache_common::source::SourceData;
use aurcache_db::helpers::operations;
use aurcache_db::prelude::Packages;
use aurcache_utils::publish;
use aurcache_utils::repository::{REPO_ROOT, Repository};
use aurcache_utils::snapshot::SnapshotStore;
use sea_orm::{DatabaseConnection, EntityTrait};
use std::sync::Arc;
use tracing::{info, warn};

const START_BANNER: &str = r"
          _    _ _____   _____           _
     /\  | |  | |  __ \ / ____|         | |
    /  \ | |  | | |__) | |     __ _  ___| |__   ___
   / /\ \| |  | |  _  /| |    / _` |/ __| '_ \ / _ \
  / ____ \ |__| | | \ \| |___| (_| | (__| | | |  __/
 /_/    \_\____/|_|  \_\\_____\__,_|\___|_| |_|\___|
";

pub fn pre_startup_tasks(version: &str) -> std::io::Result<()> {
    info!("{START_BANNER}");
    info!("Version: {version}");

    #[cfg(debug_assertions)]
    warn!("This is a dev build! Consider using a stable release.");

    adopt_hybrid_dirs()?;
    warn_about_ephemeral_data();
    Ok(())
}

/// Decide where the CA and the build logs live: see
/// [`aurcache_common::fs::adopt_hybrid_dirs`].
///
/// Before the database: the 0.5 conversion writes build logs, and they belong
/// wherever these end up.
fn adopt_hybrid_dirs() -> std::io::Result<()> {
    use aurcache_common::fs::{Adopted, Adoption};

    for Adopted { dir, outcome } in aurcache_common::fs::adopt_hybrid_dirs(persisted)? {
        let what = dir.describe();
        let hybrid = dir.hybrid_path();
        match outcome {
            Adoption::Untouched => {}
            Adoption::Moved { to } => info!(
                "Moved {what} from {}, the hybrid image's place for it, to {}",
                hybrid.display(),
                to.display()
            ),
            Adoption::InRepository { configured } => info!(
                "Using {what} in {}, the hybrid image's place for it: {} is not \
                 persisted. Mounting a volume there moves {what} at the next start.",
                hybrid.display(),
                configured.display()
            ),
            Adoption::Conflict { configured } => warn!(
                "Using {what} in {}; the hybrid image's, in {}, is left as it was. \
                 Remove whichever is not wanted.",
                configured.display(),
                hybrid.display()
            ),
        }
    }
    Ok(())
}

/// Whether a directory, or the nearest of its parents that exists yet, is
/// on a volume rather than on the container's own layers.
fn persisted(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let Ok(root_dev) = std::fs::metadata("/").map(|m| m.dev()) else {
            return false;
        };
        let Ok(dir) = std::path::absolute(dir) else {
            return false;
        };
        dir.ancestors()
            .find_map(|dir| std::fs::metadata(dir).ok())
            .is_some_and(|m| !is_ephemeral(in_container(), root_dev, m.dev()))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        true
    }
}

/// Whether the server runs in a container: one of this project's images,
/// which say so, or one Docker or Podman started, which leave a file that does.
///
/// Erring towards "no" is the safe way round. A native service takes every
/// directory as persisted, which it is; taken for a container, it would keep
/// its state in the repository whenever that shares a filesystem with `/`.
fn in_container() -> bool {
    env::var_os("AURCACHE_CONTAINER_IMAGE").is_some()
        || Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
}

/// Whether a directory will be thrown away with the container it is in.
///
/// A mount -- volume or bind -- is a different filesystem from the image's own
/// layers, so a data directory sharing a device with `/` inside a container is
/// a data directory nobody mounted. Outside a container it shares that device
/// on any ordinary installation, and means nothing.
const fn is_ephemeral(in_container: bool, root_dev: u64, data_dev: u64) -> bool {
    in_container && root_dev == data_dev
}

/// Say so, once, when the server's data is on a container's writable layer.
///
/// It is worth saying because the loss is silent and total: the container is
/// replaced on the next image update and the packages, the database and every
/// build log go with it. Warning is deliberately preferred to declaring
/// `VOLUME /app` in the image, which would make the data survive in an
/// anonymous volume the operator cannot find, cannot back up, and loses to
/// `docker volume prune` or a service rescheduled onto another Swarm node --
/// a failure that looks like success until it does not.
fn warn_about_ephemeral_data() {
    let ephemeral: Vec<String> = [
        PathBuf::from(REPO_ROOT),
        aurcache_common::fs::build_log_root(),
        aurcache_common::fs::ca_dir(),
    ]
    .into_iter()
    .filter(|dir| !persisted(dir))
    .map(|dir| dir.display().to_string())
    .collect();

    if !ephemeral.is_empty() {
        warn!(
            "Not persisted, and lost when this container is replaced: {}. \
             Mount /app (or these paths individually) to keep them.",
            ephemeral.join(", ")
        );
    }
}

/// Remove source checkouts left behind by packages that no longer exist.
///
/// Run at boot, before the build queue, the schedulers and the API are
/// started: [`SnapshotStore::prune_orphaned_checkouts`] cannot tell a clone
/// that is in progress from one that is stranded, and at this point nothing
/// else is resolving sources.
///
/// Boot is also the only moment that catches every way a checkout is
/// stranded. Deleting a package removes its own checkout, but an add clones
/// every package it plans before writing a single row, so an add that fails
/// part-way leaves clones no row ever referred to. `/app` is persisted, so
/// nothing else will ever clear them.
pub async fn prune_source_checkouts(db: &DatabaseConnection, store: &SnapshotStore) {
    let live: Vec<SourceData> = match Packages::find().all(db).await {
        Ok(packages) => packages.into_iter().map(|pkg| pkg.source_data).collect(),
        // Without the full list, every checkout looks orphaned. Removing them
        // would be a slow, silent re-clone of everything at best.
        Err(e) => {
            warn!("could not list packages, skipping source checkout prune: {e}");
            return;
        }
    };

    match store.prune_orphaned_checkouts(&live).await {
        Ok(0) => {}
        Ok(removed) => info!("removed {removed} orphaned source checkout(s)"),
        Err(e) => warn!("source checkout prune did not complete: {e}"),
    }
}

/// Report approved workers whose certificates this server's CA did not issue.
///
/// That is a replaced CA -- most often its directory was not persisted -- and
/// nothing else says so: those workers are refused on every request, and look
/// merely idle until someone wonders why. The embedded worker is left out, as
/// it enrolls again by itself.
pub async fn report_untrusted_workers(
    db: &DatabaseConnection,
    ca: &aurcache_ca::Ca,
    activity: &ActivityLog,
) {
    let workers = match aurcache_db::helpers::worker_store::list_workers(db).await {
        Ok(workers) => workers,
        Err(e) => {
            warn!("could not check the workers against the CA: {e}");
            return;
        }
    };
    let untrusted: Vec<String> = workers
        .into_iter()
        .filter(|worker| worker.status == ApprovalStatus::Approved && !worker.embedded)
        .filter(|worker| {
            worker
                .signed_cert
                .as_deref()
                .is_some_and(|cert| !ca.issued(cert))
        })
        .map(|worker| worker.name)
        .collect();
    if !untrusted.is_empty() {
        activity.emit(Event::WorkersUntrusted { workers: untrusted });
    }
}

/// Publish again the builds a restart interrupted while they were being
/// published. Nothing about them was made public yet, and their uploads are
/// still staged, so they simply start over.
pub async fn resume_publishing(
    db: &DatabaseConnection,
    repo: &Arc<Repository>,
    activity: &ActivityLog,
) {
    let interrupted = match publish::interrupted(db, repo).await {
        Ok(ids) => ids,
        Err(e) => {
            warn!("could not look for builds a restart interrupted while publishing: {e}");
            return;
        }
    };
    for build_id in interrupted {
        info!("resuming publication of build #{build_id}");
        let (db, repo) = (db.clone(), Arc::clone(repo));
        let activity = activity.clone();
        tokio::spawn(async move {
            publish::publish_with_recovery(&db, &repo, &activity, build_id).await;
        });
    }
}

/// See [`operations::close_orphaned`]; this only reports what it did.
pub async fn close_orphaned_operations(db: &DatabaseConnection) {
    match operations::close_orphaned(db).await {
        Ok(0) => {}
        Ok(closed) => warn!("Closed {closed} operation(s) left running by a restart"),
        Err(e) => warn!("could not close interrupted operations: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::is_ephemeral;

    /// Sharing a device with `/` means "nobody mounted this" only inside a
    /// container. On a normal install it is simply where the data lives.
    #[test]
    fn only_an_unmounted_container_path_is_ephemeral() {
        assert!(is_ephemeral(true, 55, 55));
        assert!(!is_ephemeral(true, 55, 64768));
        assert!(!is_ephemeral(false, 55, 55));
    }
}
