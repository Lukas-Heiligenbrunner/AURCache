//! The settings the `devtools` chroot executor declares, beside the protocol
//! ones every worker has.
//!
//! Split this way because it is the split that matters: the container executor
//! reads `CPU_LIMIT` in milli-CPUs and `MEMORY_LIMIT` in MB and would declare
//! exactly those, with its own descriptions, rather than having a shared
//! vocabulary translated onto it. The server learns what a worker accepts from
//! the worker.
//!
//! Only policy and tuning is here. What a build may *reach* -- the chroot
//! directory, the bind mounts, the build user, the `makechrootpkg` wrapper --
//! is not declared, because a worker runs devtools as root and the set of keys
//! the server may name is the boundary that keeps a compromised server from
//! choosing what runs where. See `design/implemented/worker-configuration.md`.

use aurcache_common::worker_config::{Applies, ValueKind};
use aurcache_worker_core::settings::{Builtin, SettingSpec};
use std::time::Duration;

/// Keys of the settings this executor accepts.
pub mod keys {
    pub const KEYSERVER: &str = "keyserver";
    pub const CACHE_MAX_SIZE: &str = "cache_max_size";
    pub const CACHE_TTL: &str = "cache_ttl";
    pub const SRCCACHE_MAX_SIZE: &str = "srccache_max_size";
    pub const PKGCACHE_MAX_SIZE: &str = "pkgcache_max_size";
    pub const PKGCACHE_TTL: &str = "pkgcache_ttl";
    pub const CHROOT_REFRESH_INTERVAL: &str = "chroot_refresh_interval";
    pub const BUILD_MEMORY_MAX: &str = "build_memory_max";
    pub const BUILD_SWAP_MAX: &str = "build_swap_max";
    pub const BUILD_CPUS: &str = "build_cpus";
    pub const TOTAL_BUILD_MEMORY_MAX: &str = "total_build_memory_max";
    pub const TOTAL_BUILD_SWAP_MAX: &str = "total_build_swap_max";
    pub const TOTAL_BUILD_CPUS: &str = "total_build_cpus";
    pub const BUILD_DISK_MAX: &str = "build_disk_max";
    pub const DISK_MAX: &str = "disk_max";
    pub const KEEP_FAILED: &str = "keep_failed";
    pub const BUILDDIR_MAX_BYTES: &str = "builddir_max_bytes";
    pub const BUILDDIR_MIN_FREE: &str = "builddir_min_free";
}

/// Re-exported from the shared executor settings: the legacy container
/// executor's build script uses the same default, and two literals would drift.
pub use aurcache_worker_core::settings::DEFAULT_KEYSERVER;
/// Total disk the worker's caches may use when nothing is configured.
pub const DEFAULT_TOTAL_CACHE_SIZE: u64 = 20 * 1024 * 1024 * 1024;
/// How long a source stays in the cache without being used.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// How long a freshly upgraded base chroot counts as current.
///
/// The refresh costs ~13s and, measured over a day on the reference worker, 23
/// of 26 of them upgraded nothing: Arch's repositories move a few times a day,
/// not a few times an hour.
pub const DEFAULT_CHROOT_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Disk one build may use when nothing is configured.
pub const DEFAULT_BUILD_DISK_MAX: u64 = 50 * 1024 * 1024 * 1024;
/// Everything the worker may store when nothing is configured: the base
/// chroot, the builds, and (once they move into the pool) the caches.
pub const DEFAULT_DISK_MAX: u64 = 200 * 1024 * 1024 * 1024;
/// Enough for one very large build tree, small enough that opting in a second
/// is a decision someone makes.
pub const DEFAULT_BUILDDIR_MAX_BYTES: u64 = 200 * 1024 * 1024 * 1024;
/// Secondary floor, covering what a cap cannot see: a small disk, or one
/// shared with something else that grew.
pub const DEFAULT_BUILDDIR_MIN_FREE: u64 = 50 * 1024 * 1024 * 1024;

/// The settings this executor accepts, on top of the protocol ones.
#[must_use]
pub fn chroot_settings() -> Vec<SettingSpec> {
    vec![
        SettingSpec {
            key: keys::KEYSERVER,
            env_var: "WORKER_KEYSERVER",
            kind: ValueKind::Text,
            description: "Keyserver used to fetch the keys a PKGBUILD names in validpgpkeys. \
                          Which keys are accepted is pinned by the PKGBUILD, not by this.",
            category: "Signatures",
            applies: Applies::NextJob,
            default: Builtin::Text(DEFAULT_KEYSERVER),
        },
        SettingSpec {
            key: keys::CACHE_MAX_SIZE,
            env_var: "WORKER_CACHE_MAX_SIZE",
            kind: ValueKind::Size,
            description: "Total disk the source and package caches may use between them, split \
                          evenly unless a pool is pinned below. 0 disables size-based eviction.",
            category: "Caches",
            applies: Applies::NextLoop,
            default: Builtin::Size(DEFAULT_TOTAL_CACHE_SIZE),
        },
        SettingSpec {
            key: keys::SRCCACHE_MAX_SIZE,
            env_var: "WORKER_SRCCACHE_MAX_SIZE",
            kind: ValueKind::Size,
            description: "Disk the downloaded and checked-out sources may use. Unset takes half \
                          of the total cache budget.",
            category: "Caches",
            applies: Applies::NextLoop,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::PKGCACHE_MAX_SIZE,
            env_var: "WORKER_PKGCACHE_MAX_SIZE",
            kind: ValueKind::Size,
            description: "Disk the shared pacman package cache may use. Unset takes half of the \
                          total cache budget. Kept apart from the source budget so large VCS \
                          checkouts cannot starve it.",
            category: "Caches",
            applies: Applies::NextLoop,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::CACHE_TTL,
            env_var: "WORKER_CACHE_TTL",
            kind: ValueKind::Duration,
            description: "How long an unused source stays cached. 0 disables age-based \
                          eviction.",
            category: "Caches",
            applies: Applies::NextLoop,
            default: Builtin::Duration(DEFAULT_CACHE_TTL),
        },
        SettingSpec {
            key: keys::PKGCACHE_TTL,
            env_var: "WORKER_PKGCACHE_TTL",
            kind: ValueKind::Duration,
            description: "How long a cached package stays. 0 (the default) disables it: pacman \
                          does not touch a package's mtime on a cache hit, so age-evicting \
                          would discard one used daily simply for being old.",
            category: "Caches",
            applies: Applies::NextLoop,
            default: Builtin::Duration(Duration::ZERO),
        },
        SettingSpec {
            key: keys::CHROOT_REFRESH_INTERVAL,
            env_var: "WORKER_CHROOT_REFRESH_INTERVAL",
            kind: ValueKind::Duration,
            description: "How long the shared base chroot counts as current before it is \
                          upgraded again. Bounds how stale the base may be, not what a build \
                          sees: every build syncs its own copy before it starts.",
            category: "Chroots",
            applies: Applies::NextLoop,
            default: Builtin::Duration(DEFAULT_CHROOT_REFRESH_INTERVAL),
        },
        SettingSpec {
            key: keys::BUILD_MEMORY_MAX,
            env_var: "WORKER_BUILD_MEMORY_MAX",
            kind: ValueKind::Size,
            description: "Memory one build may use before it is killed. Unset is unlimited.",
            category: "Build limits",
            applies: Applies::NextJob,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::BUILD_SWAP_MAX,
            env_var: "WORKER_BUILD_SWAP_MAX",
            kind: ValueKind::Size,
            description: "Swap one build may use. Unset means none beyond its memory limit, or \
                          unlimited when there is no memory limit either.",
            category: "Build limits",
            applies: Applies::NextJob,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::BUILD_CPUS,
            env_var: "WORKER_BUILD_CPUS",
            kind: ValueKind::Float {
                min: Some(0.0),
                max: None,
            },
            description: "CPUs one build may use, fractions allowed. Unset, or 0, is \
                          unlimited.",
            category: "Build limits",
            applies: Applies::NextJob,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::TOTAL_BUILD_MEMORY_MAX,
            env_var: "WORKER_TOTAL_BUILD_MEMORY_MAX",
            kind: ValueKind::Size,
            description: "Memory all builds on this worker may use between them. Unset is \
                          unlimited. Applies to builds already running: lowering it below what \
                          they use makes the kernel reclaim, then kill.",
            category: "Build limits",
            applies: Applies::Immediately,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::TOTAL_BUILD_SWAP_MAX,
            env_var: "WORKER_TOTAL_BUILD_SWAP_MAX",
            kind: ValueKind::Size,
            description: "Swap all builds may use between them. Unset means none beyond their \
                          shared memory limit.",
            category: "Build limits",
            applies: Applies::Immediately,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::TOTAL_BUILD_CPUS,
            env_var: "WORKER_TOTAL_BUILD_CPUS",
            kind: ValueKind::Float {
                min: Some(0.0),
                max: None,
            },
            description: "CPUs all builds may use between them. Unset, or 0, is unlimited.",
            category: "Build limits",
            applies: Applies::Immediately,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::BUILD_DISK_MAX,
            env_var: "WORKER_BUILD_DISK_MAX",
            kind: ValueKind::Size,
            description: "Disk one build may write: its chroot, its sources and the packages it \
                          makes. Enforced by the kernel as the build writes, so a build past it \
                          fails with \"Disk quota exceeded\" instead of filling the host.",
            category: "Disk",
            applies: Applies::NextJob,
            default: Builtin::Size(DEFAULT_BUILD_DISK_MAX),
        },
        SettingSpec {
            key: keys::DISK_MAX,
            env_var: "WORKER_DISK_MAX",
            kind: ValueKind::Size,
            description: "Everything this worker may store in its storage pool: the base chroot \
                          and every running build. Also the size of the pool's image, which \
                          grows to follow it. Applies to builds already running.",
            category: "Disk",
            applies: Applies::Immediately,
            default: Builtin::Size(DEFAULT_DISK_MAX),
        },
        SettingSpec {
            key: keys::KEEP_FAILED,
            env_var: "WORKER_KEEP_FAILED",
            kind: ValueKind::Duration,
            description: "How long a failed build's chroot is kept for inspection (`systemd-nspawn \
                          -D <pool>/kept-<id>`). Unset keeps nothing. Canceled, timed-out and \
                          out-of-disk builds are never kept; a kept build counts against the \
                          pool's total and goes sooner if the pool needs the room.",
            category: "Chroots",
            applies: Applies::NextLoop,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::BUILDDIR_MAX_BYTES,
            env_var: "WORKER_BUILDDIR_MAX_BYTES",
            kind: ValueKind::Size,
            description: "Total disk the kept build trees may occupy before the oldest are \
                          swept.",
            category: "Build trees",
            applies: Applies::NextLoop,
            default: Builtin::Size(DEFAULT_BUILDDIR_MAX_BYTES),
        },
        SettingSpec {
            key: keys::BUILDDIR_MIN_FREE,
            env_var: "WORKER_BUILDDIR_MIN_FREE",
            kind: ValueKind::Size,
            description: "Disk to keep free on the filesystem holding the build trees, whatever \
                          the cap above allows.",
            category: "Build trees",
            applies: Applies::NextLoop,
            default: Builtin::Size(DEFAULT_BUILDDIR_MIN_FREE),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_whole_table_checks_out() {
        let all: Vec<_> = aurcache_worker_core::settings::protocol_settings()
            .into_iter()
            .chain(chroot_settings())
            .collect();
        aurcache_worker_core::settings::check_table(&all);
    }

    /// The security boundary, asserted rather than left to review: these decide
    /// what a build can reach, and a server that could name them could make a
    /// worker run a build as root or bind `/` into the chroot.
    #[test]
    fn nothing_that_decides_what_a_build_may_reach_is_declared() {
        let declared: HashSet<_> = aurcache_worker_core::settings::protocol_settings()
            .into_iter()
            .chain(chroot_settings())
            .map(|spec| spec.env_var)
            .collect();
        for forbidden in [
            "WORKER_BIND_MOUNTS",
            "WORKER_BUILD_USER",
            "WORKER_CHROOT_DIR",
            "WORKER_POOL",
            "WORKER_DISK_RESERVE",
            "WORKER_GIT_SSH_KEY",
            "WORKER_SSH_KNOWN_HOSTS",
            "WORKER_DATA_DIR",
            "AURCACHE_URL",
            "AURCACHE_ENROLLMENT_TOKEN",
            "AURCACHE_EMBEDDED_WORKER_SECRET",
            "AURCACHE_ENROLLMENT_DIR",
            "AURCACHE_SERVER_CA_FINGERPRINT",
            "WORKER_HEARTBEAT_INTERVAL",
        ] {
            assert!(
                !declared.contains(forbidden),
                "{forbidden} must not be settable from the server"
            );
        }
    }
}
