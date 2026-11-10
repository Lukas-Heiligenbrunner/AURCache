//! The server's mirrorlists, one per architecture: who supplies each, and the
//! ranking that keeps x86_64's current when nothing else does.
//!
//! An architecture's mirrorlist comes from, in order:
//!
//! 1. `MIRRORLIST_SERVERS_<ARCH>`, written over it at every start;
//! 2. a mounted `mirrorlist`, copied into the native architecture's slot at
//!    every start;
//! 3. for x86_64 alone, ranking -- an unranked list at the first start, ranked
//!    on schedule after that. Every other architecture is configured or
//!    absent, and absent is fine: the worker then uses its own image's.

use crate::{Wake, sleep_until_next_fire};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::server_env::{self, Var};
use aurcache_utils::job_config::{
    mirrorlist_dir, mirrorlist_path, mounted_mirrorlist, native_arch,
};
use aurcache_utils::scheduled::{Job, schedule};
use pacman_mirrors::benchmark::{Bench, gen_mirrorlist};
use pacman_mirrors::platforms::Platform;
use std::time::Duration;
use tokio::fs;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// The architecture `pacman_mirrors` can rank for.
const RANKED: Platform = Platform::X86_64;

/// Write every mirrorlist the server is given, and an unranked one for
/// [`RANKED`] where nothing gave one.
///
/// Run at startup, before anything resolves dependencies: official-repo
/// lookups -- what tells `glibc` from an AUR package -- need a mirrorlist, and
/// ranking one takes far longer than fetching the mirror status.
///
/// # Errors
///
/// A configured mirrorlist could not be written. A mount that cannot be
/// adopted or a seed that cannot be fetched only leaves a slot empty, which
/// every reader tolerates, and is logged.
pub async fn prepare() -> anyhow::Result<()> {
    let dir = mirrorlist_dir();
    if let Err(e) = fs::create_dir_all(&dir).await {
        warn!("Failed to create mirrorlist dir {}: {e}", dir.display());
    }
    adopt_mount().await;
    write_configured().await?;
    if let Err(e) = seed().await {
        warn!("Failed to write an initial mirrorlist: {e:#}");
    }
    Ok(())
}

/// Rank [`RANKED`]'s mirrors on `MIRROR_RANK_SCHEDULE`, when nothing else
/// supplies its mirrorlist.
///
/// `None` when ranking does not run at all: switched off, or the list is the
/// operator's.
///
/// # Errors
///
/// The schedule does not parse; the activity log says so too.
pub fn start_ranking(activity: ActivityLog) -> anyhow::Result<Option<JoinHandle<()>>> {
    match supplier(
        configured(RANKED).is_some(),
        mounted_mirrorlist().is_some(),
        native_arch(),
    ) {
        Supplier::Environment => {
            info!("Mirror ranking is off: {} is set", env_var(RANKED));
            return Ok(None);
        }
        Supplier::Mount => {
            info!("Mirror ranking is off: a mounted mirrorlist is the {RANKED} one");
            return Ok(None);
        }
        Supplier::Ranking => {}
    }

    // Once a week, on a day and at a time of its own: mirrors see every
    // instance ranking them, and the hashed `@weekly` keeps them from all
    // doing it at once.
    let cron_str = aurcache_common::server_env::MIRROR_RANK_SCHEDULE
        .get()
        .unwrap_or_else(|_| "@weekly".to_string());
    if aurcache_common::schedule::is_off(&cron_str) {
        info!("Mirror ranking is off (MIRROR_RANK_SCHEDULE={cron_str:?})");
        return Ok(None);
    }
    let read = schedule(Job::MirrorRanking, &cron_str).inspect_err(|e| {
        // In the activity log as well as the error: a schedule in a syntax
        // this version does not read is otherwise only a startup log line,
        // and ranking stops.
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

    Ok(Some(tokio::spawn(async move {
        let mut reported = false;
        loop {
            // A schedule with no next run (`0 0 31 2 *`) waits and says so.
            // Fixed at startup, so there is nothing to read again meanwhile.
            if sleep_until_next_fire(&schedule, "mirror ranking", Duration::MAX).await
                == Wake::Fired
            {
                match rank().await {
                    Ok(()) => info!("Mirror ranking finished"),
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
    })))
}

/// Who supplies [`RANKED`]'s mirrorlist.
#[derive(Debug, PartialEq, Eq)]
enum Supplier {
    /// `MIRRORLIST_SERVERS_X86_64`.
    Environment,
    /// A mounted `mirrorlist`, on an x86_64 host.
    Mount,
    /// Nobody: ranking writes it.
    Ranking,
}

/// Who supplies [`RANKED`]'s mirrorlist, given whether its variable is set,
/// whether a mirrorlist is mounted, and the host's architecture.
///
/// A mount is adopted into the *native* architecture's slot, so it is
/// [`RANKED`]'s only on an x86_64 host; ranking it anyway would overwrite the
/// operator's mirrors, and the next start copy them back, for ever. Ranking
/// cannot start from a supplied list instead: it starts from Arch's
/// mirror-status API and needs per-mirror metadata a mirrorlist does not carry.
fn supplier(configured: bool, mounted: bool, native_arch: &str) -> Supplier {
    if configured {
        Supplier::Environment
    } else if mounted && native_arch == RANKED.as_str() {
        Supplier::Mount
    } else {
        Supplier::Ranking
    }
}

/// The environment variable naming an architecture's mirrorlist servers.
fn env_var(platform: Platform) -> Var {
    match platform {
        Platform::X86_64 => server_env::MIRRORLIST_SERVERS_X86_64,
        Platform::Aarch64 => server_env::MIRRORLIST_SERVERS_AARCH64,
        Platform::Armv7h => server_env::MIRRORLIST_SERVERS_ARMV7H,
    }
}

/// The servers configured for `platform`, `;`-separated, if any.
fn configured(platform: Platform) -> Option<String> {
    env_var(platform)
        .get()
        .ok()
        .filter(|servers| !servers.trim().is_empty())
}

/// Copy a mounted `mirrorlist` into this host's `mirrorlist.<arch>` slot.
///
/// Mirrorlists are stored per architecture, but mounting a single file is the
/// convenient thing to do, and what a 0.5 deployment has. Adopting at boot
/// means every reader deals only with the arch-keyed layout instead of each
/// carrying a fallback.
///
/// The copy is unconditional, so editing the mounted file takes effect on the
/// next boot with no staleness bookkeeping to get wrong. Safe to clobber the
/// target: ranking stands down whenever the mount owns its slot (see
/// [`supplier`]).
async fn adopt_mount() {
    let Some(shared) = mounted_mirrorlist() else {
        return;
    };
    let target = mirrorlist_path(native_arch());
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

/// Write `MIRRORLIST_SERVERS_<ARCH>` for every architecture AURCache builds
/// for, not x86_64 alone.
///
/// Arch and Arch Linux ARM do not share a URL layout -- `$repo/os/$arch`
/// against `$arch/$repo` -- so an ARM mirrorlist cannot be derived from the
/// x86_64 one by substituting the architecture; it has to be configured
/// separately or not at all.
async fn write_configured() -> anyhow::Result<()> {
    for platform in Platform::ALL {
        let Some(servers) = configured(platform) else {
            continue;
        };
        let path = mirrorlist_path(platform.as_str());
        let mirrorlist = servers
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("Server = {s}\n"))
            .collect::<String>();
        fs::write(&path, mirrorlist).await?;
        info!(
            "Wrote {platform} mirrorlist from {} to {}",
            env_var(platform),
            path.display()
        );
    }
    Ok(())
}

/// Write an unranked [`RANKED`] mirrorlist when there is none yet. The
/// scheduled ranking replaces it on its next run.
async fn seed() -> anyhow::Result<()> {
    let target = mirrorlist_path(RANKED.as_str());
    if fs::try_exists(&target).await.unwrap_or(false) {
        return Ok(());
    }
    info!("Perform initial load of pacman mirrorlist");
    let status = pacman_mirrors::get_status(RANKED).await?;
    fs::write(&target, gen_mirrorlist(&status.urls.0)).await?;
    info!("Wrote mirrorlist to {}", target.display());
    Ok(())
}

/// Rank mirrors and write them to [`RANKED`]'s mirrorlist.
async fn rank() -> anyhow::Result<()> {
    info!("Executing mirror ranking job");
    let urls = pacman_mirrors::get_status(RANKED).await?.urls;

    info!("Ranking mirrorlist");
    let mirrorlist = gen_mirrorlist(&urls.rank().await?);

    fs::create_dir_all(mirrorlist_dir()).await?;
    let target = mirrorlist_path(RANKED.as_str());
    fs::write(&target, mirrorlist).await?;
    info!("Wrote mirrorlist to {}", target.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variable wins over everything: it is written over the slot at
    /// every start.
    #[test]
    fn a_configured_mirrorlist_stops_ranking() {
        assert_eq!(supplier(true, true, "x86_64"), Supplier::Environment);
        assert_eq!(supplier(true, false, "aarch64"), Supplier::Environment);
    }

    /// On x86_64 the mounted list and the ranked list are the same file, so
    /// ranking must stand down or the two writers overwrite each other forever.
    #[test]
    fn a_mounted_mirrorlist_stops_ranking_on_the_native_arch() {
        assert_eq!(supplier(false, true, "x86_64"), Supplier::Mount);
    }

    /// On any other host the mount lands in a different arch's slot, so ranking
    /// x86_64 clobbers nothing and should still run.
    #[test]
    fn a_mount_for_another_arch_does_not_stop_ranking() {
        assert_eq!(supplier(false, true, "aarch64"), Supplier::Ranking);
        assert_eq!(supplier(false, true, "armv7h"), Supplier::Ranking);
    }

    #[test]
    fn ranking_runs_when_nothing_is_supplied() {
        assert_eq!(supplier(false, false, "x86_64"), Supplier::Ranking);
        assert_eq!(supplier(false, false, "aarch64"), Supplier::Ranking);
    }

    /// The x86_64 name is the one already documented and deployed; the others
    /// follow the same rule.
    #[test]
    fn env_var_names_follow_the_architecture() {
        for platform in Platform::ALL {
            let expected = format!("MIRRORLIST_SERVERS_{}", platform.as_str().to_uppercase());
            assert_eq!(env_var(platform).name(), expected);
        }
    }
}
