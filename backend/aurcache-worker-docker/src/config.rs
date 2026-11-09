//! Configuration for the legacy container executor.
//!
//! These are the pre-worker environment variables, read under their original
//! names and with their original meanings, because the whole point of this
//! executor is that an existing deployment keeps behaving as it did. The two
//! limits are declared settings as well (see [`crate::settings`]), so they can
//! also be set from the server -- `CPU_LIMIT` still pins, as it always did.

use crate::settings::keys;
use aurcache_worker_core::config::{CoreConfig, env_opt};
use aurcache_worker_core::settings::WorkerSettings;
use std::path::PathBuf;

/// Where a build directory lives, from two points of view.
///
/// In the single-container topology the worker writes into a path inside its
/// own filesystem, but the Docker daemon binds the *host* path — they are the
/// same directory reached by different routes, and confusing them yields a
/// container with an empty bind mount and no explanation.
#[derive(Clone, Debug)]
pub struct BuildDirs {
    /// Path as the Docker daemon sees it (`BUILD_ARTIFACT_DIR`).
    pub host: PathBuf,
    /// Path as this process sees it.
    pub local: PathBuf,
}

/// Fully-resolved configuration for the container executor.
#[derive(Clone, Debug)]
pub struct Config {
    /// Image each build container is created from (`BUILDER_IMAGE`).
    pub builder_image: String,
    /// Shared build directory, host and local views.
    pub dirs: BuildDirs,
    /// The declared settings, as read from the worker's resolution of them.
    pub tunables: Tunables,
}

/// The limits each build container gets: the two settings this executor
/// declares, read whole from the declared settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tunables {
    /// CPU limit in milli-CPUs, `0` for unlimited (`CPU_LIMIT`).
    pub cpu_limit: u64,
    /// Memory limit in MB, negative for unlimited (`MEMORY_LIMIT`).
    pub memory_limit: i64,
}

/// The platforms the pre-worker builder built for: `pacman_mirrors::Platform::ALL`,
/// spelled out because that crate would bring a database driver with it.
const PLATFORMS: [&str; 3] = ["x86_64", "aarch64", "armv7h"];

/// The default builder image. Not the pre-worker `aurcache-builder`: 0.5.0
/// keeps pulling that before every build and runs paru in it, which this
/// image does not ship.
const DEFAULT_BUILDER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest";

/// The protocol configuration for this executor, its settings declared
/// beside the protocol's so registration reports one table.
#[must_use]
pub fn core_from_env() -> CoreConfig {
    let mut core = CoreConfig::from_env(crate::settings::docker_settings());
    // Every platform unless told otherwise, as the pre-worker builder did:
    // each build runs in an image pulled for its platform, and the host's
    // binfmt runs a foreign one. Checking for the handlers instead would
    // stop builds that used to work, since a container rarely sees them.
    if env_opt("WORKER_ARCHES").is_none() {
        core.arches = PLATFORMS.iter().map(ToString::to_string).collect();
    }
    core
}

impl Config {
    /// Build a [`Config`] from the process environment and `settings`, the
    /// worker's resolution of its declared ones.
    ///
    /// Returns `None` when `BUILD_ARTIFACT_DIR` is unset: without a host path
    /// for the bind mount there is nothing this executor can do, and guessing
    /// one would produce containers that silently build into nowhere.
    #[must_use]
    pub fn from_env(settings: &WorkerSettings) -> Option<Self> {
        let host = env_opt("BUILD_ARTIFACT_DIR").map(PathBuf::from)?;
        let local = env_opt("BUILD_ARTIFACT_DIR_LOCAL")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("builds")
            });
        Some(Self {
            builder_image: env_opt("BUILDER_IMAGE")
                .unwrap_or_else(|| DEFAULT_BUILDER_IMAGE.to_string()),
            dirs: BuildDirs { host, local },
            tunables: Tunables::from_settings(settings),
        })
    }

    /// The same configuration with `settings` in force: the limits read
    /// again from them, and nothing else touched -- the image and the build
    /// directory are the machine's.
    #[must_use]
    pub fn with_settings(&self, settings: &WorkerSettings) -> Self {
        Self {
            tunables: Tunables::from_settings(settings),
            ..self.clone()
        }
    }
}

impl Tunables {
    /// Read the limits from `settings`.
    #[must_use]
    pub fn from_settings(settings: &WorkerSettings) -> Self {
        Self {
            // The declared minimum of 0 keeps a negative CPU limit from ever
            // getting here.
            cpu_limit: u64::try_from(
                settings
                    .integer(keys::CPU_LIMIT)
                    .unwrap_or(crate::settings::DEFAULT_CPU_LIMIT),
            )
            .unwrap_or_default(),
            // Negative is unlimited for memory, as it always was.
            memory_limit: settings
                .integer(keys::MEMORY_LIMIT)
                .unwrap_or(crate::settings::DEFAULT_MEMORY_LIMIT),
        }
    }

    /// Docker's `NanoCpus`, or `None` for unlimited.
    #[must_use]
    pub fn nano_cpus(&self) -> Option<i64> {
        // Saturated, not panicking: these are operator-controlled numbers and
        // the multiplication would otherwise overflow (and panic in debug)
        // on absurd input — including via the `i64::MAX` fallback itself.
        (self.cpu_limit > 0).then(|| {
            i64::try_from(self.cpu_limit)
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000_000)
        })
    }

    /// The limit in bytes, for Docker's `Memory` and `MemorySwap` alike, or
    /// `None` for unlimited.
    #[must_use]
    pub fn memory_bytes(&self) -> Option<i64> {
        (self.memory_limit > 0).then(|| self.memory_limit.saturating_mul(1024 * 1024))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn cfg(cpu_limit: u64, memory_limit: i64) -> Tunables {
        Tunables {
            cpu_limit,
            memory_limit,
        }
    }

    /// The pre-worker defaults (`0` CPU, `-1` memory) meant "unlimited"; they
    /// must not be passed to Docker as a literal zero, which means something
    /// else entirely.
    #[test]
    fn zero_and_negative_limits_mean_unlimited() {
        assert_eq!(cfg(0, -1).nano_cpus(), None);
        assert_eq!(cfg(0, -1).memory_bytes(), None);
    }

    #[test]
    fn limits_convert_to_docker_units() {
        assert_eq!(cfg(2000, 512).nano_cpus(), Some(2_000_000_000));
        assert_eq!(cfg(2000, 512).memory_bytes(), Some(512 * 1024 * 1024));
    }

    /// A limit set on the server reaches the next container in Docker's units,
    /// and removing it goes back to the old default of unlimited.
    ///
    /// Reads the process environment, as the worker does, so it asserts nothing
    /// on a machine that pins either limit -- a pin outranks the server, which
    /// is the point of one.
    #[test]
    fn a_delivered_limit_reaches_the_next_container() {
        use aurcache_common::worker_config::ConfigSnapshot;
        let pinned: Vec<_> = ["CPU_LIMIT", "MEMORY_LIMIT"]
            .into_iter()
            .filter(|var| std::env::var_os(var).is_some())
            .collect();
        if !pinned.is_empty() {
            eprintln!("skipped: {pinned:?} set in this environment");
            return;
        }
        let snapshot = |pairs: &[(&str, &str)]| ConfigSnapshot {
            revision: format!("{pairs:?}"),
            settings: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        };
        let declared =
            WorkerSettings::from_env(aurcache_worker_core::settings::protocol_settings())
                .extended(crate::settings::docker_settings());
        assert_eq!(Tunables::from_settings(&declared), cfg(0, -1));

        let set = declared.with_snapshot(&snapshot(&[
            ("cpu_limit", "1500"),
            ("memory_limit", "2048"),
        ]));
        let limits = Tunables::from_settings(&set);
        assert_eq!(limits.nano_cpus(), Some(1_500_000_000));
        assert_eq!(limits.memory_bytes(), Some(2048 * 1024 * 1024));

        let removed = Tunables::from_settings(&set.with_snapshot(&snapshot(&[])));
        assert_eq!(removed.nano_cpus(), None);
        assert_eq!(removed.memory_bytes(), None);
    }

    /// Absurd operator input saturates instead of overflowing: the old
    /// multiplication panicked in debug builds and wrapped in release.
    #[test]
    fn absurd_limits_saturate_rather_than_overflow() {
        assert_eq!(cfg(u64::MAX, 1).nano_cpus(), Some(i64::MAX));
        assert_eq!(cfg(1, i64::MAX).memory_bytes(), Some(i64::MAX));
    }
}
