use crate::{Wake, sleep_until_next_fire};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_utils::job_config::{
    mirrorlist_dir, mirrorlist_path, native_arch, shared_mirrorlist_path,
};
use aurcache_utils::scheduled::{Job, schedule};
use pacman_mirrors::benchmark::{Bench, gen_mirrorlist};
use pacman_mirrors::platforms::Platform;
use std::env;
use std::time::Duration;
use tokio::fs;
use tokio::task::JoinHandle;
use tracing::info;

pub fn start_mirror_rank_job(activity: ActivityLog) -> anyhow::Result<JoinHandle<()>> {
    // Once a week, on a day and at a time of its own: mirrors see every
    // instance ranking them, and the hashed `@weekly` keeps them from all
    // doing it at once.
    let cron_str = env::var("MIRROR_RANK_SCHEDULE").unwrap_or_else(|_| "@weekly".to_string());
    if aurcache_common::schedule::is_off(&cron_str) {
        info!("Mirror ranking is off (MIRROR_RANK_SCHEDULE={cron_str:?})");
        return Ok(tokio::spawn(async {}));
    }
    let read = schedule(Job::MirrorRanking, &cron_str).inspect_err(|e| {
        // In the activity log as well as the error: a schedule left in the
        // old syntax is otherwise only a startup log line, and ranking stops.
        activity.emit(Event::ScheduleInvalid {
            job: Job::MirrorRanking.name().to_string(),
            error: e.to_string(),
        });
    })?;
    if let Some(read_as) = &read.rewritten {
        activity.emit(Event::ScheduleOutdated {
            job: Job::MirrorRanking.name().to_string(),
            written: cron_str.clone(),
            read_as: read_as.clone(),
        });
    }
    let schedule = read.schedule;

    Ok(tokio::spawn(async move {
        let mut reported = false;
        loop {
            // A schedule with no next run (`0 0 31 2 *`) waits and says so.
            // Fixed at startup, so there is nothing to read again meanwhile.
            if sleep_until_next_fire(&schedule, "mirror ranking", Duration::MAX).await
                == Wake::Fired
            {
                match update_mirrorlist().await {
                    Ok(()) => {
                        info!("Mirror ranking finished");
                    }
                    Err(e) => activity.emit(Event::MirrorRankFailed {
                        error: format!("{e:#}"),
                    }),
                }
            } else {
                // Once: this schedule is fixed at startup, so it will not
                // start firing later.
                if !reported {
                    activity.emit(Event::ScheduleInvalid {
                        job: Job::MirrorRanking.name().to_string(),
                        error: format!("'{cron_str}' never fires again"),
                    });
                    reported = true;
                }
                tokio::time::sleep(Duration::from_secs(60 * 30)).await;
            }
        }
    }))
}

/// The architecture `pacman_mirrors` can rank for; mirrorlists are per-arch.
const RANKED_ARCH: &str = "x86_64";

/// Write an unranked [`RANKED_ARCH`] mirrorlist when there is none yet.
///
/// Run at startup, before anything resolves dependencies: official-repo
/// lookups -- what tells `glibc` from an AUR package -- need a mirrorlist, and
/// ranking one takes far longer than fetching the mirror status. The
/// scheduled ranking replaces it on its next run. Every other architecture is
/// configured or absent, and absent is fine: the worker then uses its own
/// image's mirrorlist.
pub async fn seed_mirrorlist() -> anyhow::Result<()> {
    let target = mirrorlist_path(RANKED_ARCH);
    if fs::try_exists(&target).await.unwrap_or(false) {
        return Ok(());
    }
    info!("Perform initial load of pacman mirrorlist");
    let status = pacman_mirrors::get_status(Platform::X86_64).await?;
    fs::write(&target, gen_mirrorlist(&status.urls.0)).await?;
    info!("Wrote mirrorlist to {}", target.display());
    Ok(())
}

/// Whether a mounted mirrorlist owns the slot ranking would write.
///
/// A mounted `mirrorlist` is adopted into the *native* architecture's slot at
/// startup, and ranking writes [`RANKED_ARCH`]. They are the same file only on
/// an x86_64 host, which is the sole case where the two writers would fight —
/// ranking overwriting the operator's mirrors, startup copying them back on the
/// next boot, forever.
///
/// Pure so the precedence rule is testable without a filesystem.
fn mount_owns_ranked_slot(shared_mirrorlist_exists: bool, native_arch: &str) -> bool {
    shared_mirrorlist_exists && native_arch == RANKED_ARCH
}

/// Rank mirrors and write them to [`RANKED_ARCH`]'s mirrorlist.
///
/// Stands down when the operator mounted their own mirrorlist for this
/// architecture: AURCache cannot currently rank a supplied list (ranking starts
/// from Arch's mirror-status API and needs per-mirror metadata a mirrorlist file
/// does not carry), so the only coherent options are "use their mirrors" or
/// "replace them", and an explicit mount is the stronger signal.
async fn update_mirrorlist() -> anyhow::Result<()> {
    if mount_owns_ranked_slot(shared_mirrorlist_path().exists(), native_arch()) {
        info!(
            "Skipping mirror ranking: {} is provided and adopted as the {RANKED_ARCH} mirrorlist",
            shared_mirrorlist_path().display()
        );
        return Ok(());
    }

    info!("Executing mirror ranking job");
    let urls = pacman_mirrors::get_status(Platform::X86_64).await?.urls;

    info!("Ranking mirrorlist");
    let mirrorlist = gen_mirrorlist(&urls.rank().await?);

    fs::create_dir_all(mirrorlist_dir()).await?;
    // Ranking is x86_64-only (see `Platform::X86_64` above), so this writes the
    // x86_64 slot specifically rather than the host's native one.
    let target = mirrorlist_path(RANKED_ARCH);
    fs::write(&target, mirrorlist).await?;
    info!("Wrote mirrorlist to {}", target.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On x86_64 the mounted list and the ranked list are the same file, so
    /// ranking must stand down or the two writers overwrite each other forever.
    #[test]
    fn a_mounted_mirrorlist_stops_ranking_on_the_native_arch() {
        assert!(mount_owns_ranked_slot(true, "x86_64"));
    }

    /// On any other host the mount lands in a different arch's slot, so ranking
    /// x86_64 clobbers nothing and should still run.
    #[test]
    fn a_mount_for_another_arch_does_not_stop_ranking() {
        assert!(!mount_owns_ranked_slot(true, "aarch64"));
        assert!(!mount_owns_ranked_slot(true, "armv7h"));
    }

    #[test]
    fn ranking_runs_when_nothing_is_mounted() {
        assert!(!mount_owns_ranked_slot(false, "x86_64"));
        assert!(!mount_owns_ranked_slot(false, "aarch64"));
    }
}
