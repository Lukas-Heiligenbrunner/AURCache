use aurcache_activitylog::activity_utils::ActivityLog;
use std::env;
use std::path::{Path, PathBuf};
use tokio::fs;

use aurcache_activitylog::events::Event;
use aurcache_common::api::worker::ApprovalStatus;
use aurcache_common::source::SourceData;
use aurcache_db::helpers::operations;
use aurcache_db::prelude::Packages;
use aurcache_utils::job_config::{self, mirrorlist_dir, native_arch, shared_mirrorlist_path};
use aurcache_utils::publish;
use aurcache_utils::repository::{REPO_ROOT, Repository};
use aurcache_utils::snapshot::SnapshotStore;
use pacman_mirrors::platforms::Platform;
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

pub fn pre_startup_tasks(version: &str) {
    info!("{START_BANNER}");
    info!("Version: {version}");

    #[cfg(debug_assertions)]
    warn!("This is a dev build! Consider using a stable release.");

    warn_about_ephemeral_data();
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let in_container =
            Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists();
        let Ok(root_dev) = std::fs::metadata("/").map(|m| m.dev()) else {
            return;
        };

        let ephemeral: Vec<String> = [
            PathBuf::from(REPO_ROOT),
            aurcache_common::fs::build_log_root(),
            aurcache_common::fs::ca_dir(),
        ]
        .into_iter()
        .filter(|dir| {
            std::fs::metadata(dir)
                .map(|m| is_ephemeral(in_container, root_dev, m.dev()))
                .unwrap_or(false)
        })
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
}

/// Copy a mounted `mirrorlist` into this host's `mirrorlist.<arch>` slot.
///
/// Mirrorlists are stored per-architecture, but mounting a single file is both
/// the convenient thing to do and what deployments predating the arch-keyed
/// layout already have. Adopting at boot means every reader deals only with the
/// arch-keyed layout instead of each carrying a fallback.
///
/// The copy is unconditional, so editing the mounted file takes effect on the
/// next boot with no staleness bookkeeping to get wrong. Re-copying identical
/// bytes costs nothing at this size, which is a better trade than tracking
/// provenance hashes to avoid it.
///
/// Safe to clobber the target because mirror ranking stands down whenever a
/// mount owns that architecture's slot (see `aurcache_scheduler::mirror_ranking`),
/// so nothing else writes the file this would overwrite.
///
/// Best-effort — failure just leaves the mirrorlist unpopulated, which every
/// caller already tolerates.
async fn adopt_shared_mirrorlist() {
    let shared = shared_mirrorlist_path();
    if !fs::try_exists(&shared).await.unwrap_or(false) {
        return;
    }
    let target = job_config::mirrorlist_path(native_arch());
    match fs::copy(&shared, &target).await {
        Ok(_) => info!(
            "Adopted mounted mirrorlist {} as {}",
            shared.display(),
            target.display()
        ),
        Err(e) => warn!(
            "Failed to adopt mounted mirrorlist {}: {e}",
            shared.display()
        ),
    }
}

/// Housekeeping once the database is up.
///
/// Builds in flight are left as they are. Builds run on workers, which carry on
/// through a server restart: an `ACTIVE` build is still building, and the lease
/// reaper deals with one whose worker really is gone; an `ENQUEUED` build is
/// still waiting for one. Failing them here -- what this did when builds ran
/// inside the server -- failed every build under way on every redeploy.
pub async fn post_startup_tasks(db: &DatabaseConnection) -> anyhow::Result<()> {
    close_orphaned_operations(db).await;

    let mirrorlist_dir = mirrorlist_dir();
    if let Err(e) = fs::create_dir_all(&mirrorlist_dir).await {
        warn!(
            "Failed to create mirrorlist dir {}: {e}",
            mirrorlist_dir.display()
        );
    }
    // A single mounted `mirrorlist` is the convenient thing for an operator to
    // provide, and is what every deployment predating the arch-keyed layout
    // already has. Adopt it into this host's slot so every reader can assume
    // `mirrorlist.<arch>` and no fallback logic is needed anywhere else.
    adopt_shared_mirrorlist().await;

    // `MIRRORLIST_SERVERS_<ARCH>` for any architecture AURCache builds for, not
    // x86_64 alone. Arch and Arch Linux ARM do not share a URL layout --
    // `$repo/os/$arch` against `$arch/$repo` -- so an ARM mirrorlist cannot be
    // derived from the x86_64 one by substituting the architecture; it has to
    // be configured separately or not at all.
    for platform in Platform::ALL {
        let arch = platform.as_str();
        let Some(servers) = env::var(mirrorlist_env_var(arch))
            .ok()
            .filter(|s| !s.trim().is_empty())
        else {
            continue;
        };
        let path = job_config::mirrorlist_path(arch);
        let mirrorlist = servers
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("Server = {s}\n"))
            .collect::<String>();
        fs::write(&path, mirrorlist).await?;
        info!(
            "Wrote {arch} mirrorlist from {} to {}",
            mirrorlist_env_var(arch),
            path.display()
        );
    }

    if let Err(e) = aurcache_scheduler::mirror_ranking::seed_mirrorlist().await {
        warn!("Failed to write an initial mirrorlist: {e:#}");
    }

    Ok(())
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
async fn close_orphaned_operations(db: &DatabaseConnection) {
    match operations::close_orphaned(db).await {
        Ok(0) => {}
        Ok(closed) => warn!("Closed {closed} operation(s) left running by a restart"),
        Err(e) => warn!("could not close interrupted operations: {e}"),
    }
}

/// The environment variable naming an architecture's mirrorlist servers.
///
/// `x86_64` -> `MIRRORLIST_SERVERS_X86_64`, which is the name deployments
/// already set, so generalising costs no existing configuration.
fn mirrorlist_env_var(arch: &str) -> String {
    format!("MIRRORLIST_SERVERS_{}", arch.to_uppercase())
}

#[cfg(test)]
mod mirrorlist_env_tests {
    use super::mirrorlist_env_var;

    /// The x86_64 name is the one already documented and deployed; the others
    /// follow the same rule rather than being spelled out by hand.
    #[test]
    fn env_var_names_follow_the_architecture() {
        assert_eq!(mirrorlist_env_var("x86_64"), "MIRRORLIST_SERVERS_X86_64");
        assert_eq!(mirrorlist_env_var("aarch64"), "MIRRORLIST_SERVERS_AARCH64");
        assert_eq!(mirrorlist_env_var("armv7h"), "MIRRORLIST_SERVERS_ARMV7H");
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
