//! Configuration specific to the `devtools` chroot executor.
//!
//! Protocol settings (identity, enrollment, scheduling, liveness) live in
//! [`CoreConfig`]; this adds only what building in a chroot needs. Both are
//! read from the same environment, so a worker is still configured as one flat
//! set of variables.

use crate::settings::keys;
use aurcache_worker_core::config::{CoreConfig, env_opt};
use aurcache_worker_core::settings::WorkerSettings;
use std::path::{Path, PathBuf};

/// Fully-resolved configuration for the chroot worker.
#[derive(Clone, Debug)]
pub struct Config {
    /// Settings shared with every executor.
    pub core: CoreConfig,
    /// Explicit SSH key for authenticated sources. When set, no key is
    /// generated — see `credentials.rs`.
    pub git_ssh_key: Option<PathBuf>,
    /// Optional `known_hosts` to trust inside the build chroot.
    pub ssh_known_hosts: Option<PathBuf>,
    /// Extra `host:chroot` bind mounts exposed to every build, for credentials
    /// that are not SSH (a `.netrc`, an API token, a licence file).
    pub bind_mounts: Vec<crate::build::BindMount>,
    /// Directory the storage pool lives under by default: its image and its
    /// mount point.
    pub chroot_dir: PathBuf,
    /// What backs the storage pool every chroot and build lives in
    /// (`WORKER_POOL`, `WORKER_DISK_RESERVE`). The machine's alone: it decides
    /// what the worker formats and mounts, so the server can never name it.
    pub pool_backing: aurcache_chroot::Backing,
    /// Where the pool is mounted.
    pub pool_mountpoint: PathBuf,
    /// Directory holding worker-local caches (srcdest, gnupg, pacman pkg): a
    /// subvolume in the storage pool, so the caches count against
    /// `WORKER_DISK_MAX` with everything else the worker stores.
    pub cache_dir: PathBuf,
    /// Keyserver for `gpg --recv-keys`.
    pub keyserver: String,
    /// Unix user each build runs as.
    ///
    /// Must differ from the user the worker itself runs as: the worker's mTLS
    /// identity and build credentials are protected from PKGBUILD code by file
    /// ownership, which only separates them if the two users differ.
    pub build_user: String,
    /// Source (`SRCDEST`) cache budget in bytes (`0` disables size-based
    /// eviction).
    pub cache_max_size: u64,
    /// Cache entry TTL in seconds (`0` disables age-based eviction).
    pub cache_ttl: u64,
    /// Shared pacman package cache budget in bytes (`0` disables size-based
    /// eviction). Kept separate from the source budget: the two pools have
    /// very different sizes and refill costs, and a shared budget would let
    /// large VCS checkouts starve the package cache (or vice versa).
    pub pkgcache_max_size: u64,
    /// How long a freshly `-Syu`'d base chroot counts as current, in seconds
    /// (`0` refreshes before every build, which is what this used to do).
    ///
    /// The refresh costs ~13s and, measured over a day on the reference
    /// worker, 23 of 26 of them upgraded nothing at all: Arch's repositories
    /// move a few times a day, not a few times an hour. Every build paid for
    /// that, serially, before it could start.
    ///
    /// This bounds how stale the *shared* base may be, not what a build sees:
    /// every build syncs its own chroot copy before it starts
    /// (`makechrootpkg -u`, see [`crate::build::build_command`]), because
    /// AURCache's own repository moves whenever a build finishes rather than a
    /// few times a day. Keeping the base close to current is what leaves that
    /// per-build sync with nothing to download.
    pub chroot_refresh_interval: u64,
    /// Package cache TTL in seconds. Defaults to `0` (disabled) because a
    /// cached package's mtime is its *download* time — pacman does not touch
    /// it on a cache hit — so age-evicting would discard a package used daily
    /// simply for being old. Size pressure is the honest bound for this pool.
    pub pkgcache_ttl: u64,
    /// Memory, swap and CPU each build may use (`WORKER_BUILD_MEMORY_MAX`,
    /// `WORKER_BUILD_SWAP_MAX`, `WORKER_BUILD_CPUS`). Unset means unlimited, as
    /// it always was.
    pub build_limits: crate::cgroup::BuildLimits,
    /// Memory, swap and CPU all builds may use between them
    /// (`WORKER_TOTAL_BUILD_MEMORY_MAX`, `WORKER_TOTAL_BUILD_SWAP_MAX`,
    /// `WORKER_TOTAL_BUILD_CPUS`). Unset means unlimited.
    pub total_build_limits: crate::cgroup::BuildLimits,
    /// Disk one build may write (`WORKER_BUILD_DISK_MAX`).
    pub build_disk_max: u64,
    /// Everything the worker may store in its pool (`WORKER_DISK_MAX`).
    pub disk_max: u64,
    /// How long a failed build's chroot is kept for inspection, in seconds;
    /// `None` keeps nothing (`WORKER_KEEP_FAILED`).
    pub keep_failed: Option<u64>,
}

impl Config {
    /// Build a [`Config`] from the process environment, applying zero-config
    /// defaults for everything not explicitly set.
    #[must_use]
    pub fn from_env() -> Self {
        let mut core = CoreConfig::from_env();
        // The executor's settings are declared beside the protocol ones, so
        // registration reports one table and the fields below read from the
        // same resolved values the server is shown.
        let settings =
            std::mem::take(&mut core.settings).extended(crate::settings::chroot_settings());
        let core = core.with_settings(settings);

        // The chroot lives under the data dir by default so that persisting one
        // volume persists the identity *and* the expensive base chroot; a
        // worker whose data dir moved to a bigger disk should not silently
        // leave its largest directory behind on the old one.
        let chroot_dir = env_opt("WORKER_CHROOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| core.data_dir.join("chroot"));

        let pool = env_opt("WORKER_POOL").map(PathBuf::from);
        let mut cfg = Self {
            git_ssh_key: env_opt("WORKER_GIT_SSH_KEY").map(PathBuf::from),
            ssh_known_hosts: env_opt("WORKER_SSH_KNOWN_HOSTS").map(PathBuf::from),
            bind_mounts: env_opt("WORKER_BIND_MOUNTS")
                .map(|raw| crate::credentials::parse_bind_mounts(&raw))
                .unwrap_or_default(),
            chroot_dir: chroot_dir.clone(),
            cache_dir: pool_mountpoint(pool.clone(), &chroot_dir).join(CACHE_SUBVOLUME),
            build_user: env_opt("WORKER_BUILD_USER").unwrap_or_else(|| "builder".to_string()),
            pool_backing: pool_backing(
                pool.clone(),
                env_opt("WORKER_DISK_RESERVE").is_some_and(|v| truthy(&v)),
                &chroot_dir,
            ),
            pool_mountpoint: pool_mountpoint(pool, &chroot_dir),
            // Filled in from the settings just below, by the same code that
            // fills them in again whenever the server delivers new values.
            keyserver: String::new(),
            cache_max_size: 0,
            cache_ttl: 0,
            pkgcache_max_size: 0,
            pkgcache_ttl: 0,
            chroot_refresh_interval: 0,
            build_limits: crate::cgroup::BuildLimits::default(),
            total_build_limits: crate::cgroup::BuildLimits::default(),
            build_disk_max: 0,
            disk_max: 0,
            keep_failed: None,
            core,
        };
        if env_opt("WORKER_CACHE_DIR").is_some() {
            tracing::warn!(
                "WORKER_CACHE_DIR is no longer used: the caches live in the storage pool, at {}",
                cfg.cache_dir.display()
            );
        }
        if env_opt("WORKER_CHROOT_OVERLAY").is_some() {
            tracing::warn!(
                "WORKER_CHROOT_OVERLAY is no longer used: every chroot is a snapshot in the \
                 storage pool now"
            );
        }
        cfg.read_settings();
        cfg
    }

    /// Who owns the cache subvolume: the build user and its group, as the
    /// package's tmpfiles declaration made the cache directory. Builds write
    /// sources into it as that user, and the worker through the group.
    #[must_use]
    pub fn cache_owner(&self) -> aurcache_chroot::Owner {
        aurcache_chroot::Owner::of_user(&self.build_user)
            .unwrap_or_else(aurcache_chroot::Owner::current)
    }

    /// How to open the storage pool, with the current total.
    #[must_use]
    pub fn pool_config(&self) -> aurcache_chroot::PoolConfig {
        aurcache_chroot::PoolConfig {
            backing: self.pool_backing.clone(),
            mountpoint: self.pool_mountpoint.clone(),
            total: self.disk_max,
            owner: aurcache_chroot::Owner::current(),
        }
    }

    /// The same configuration with `settings` in force: the protocol's
    /// declared fields and this executor's read again from them, and nothing
    /// else touched -- the paths, the build user and the bind mounts are the
    /// machine's alone.
    #[must_use]
    pub fn with_settings(&self, settings: WorkerSettings) -> Self {
        let mut cfg = Self {
            core: self.core.with_settings(settings),
            ..self.clone()
        };
        cfg.read_settings();
        cfg
    }

    /// Read this executor's declared fields from `core.settings`.
    fn read_settings(&mut self) {
        let settings = &self.core.settings;
        let budgets = split_cache_budgets(
            settings.size(keys::CACHE_MAX_SIZE),
            settings.size(keys::SRCCACHE_MAX_SIZE),
            settings.size(keys::PKGCACHE_MAX_SIZE),
        );
        self.keyserver = settings
            .raw(keys::KEYSERVER)
            .unwrap_or(crate::settings::DEFAULT_KEYSERVER)
            .to_string();
        self.cache_max_size = budgets.src;
        self.cache_ttl = settings
            .duration(keys::CACHE_TTL)
            .unwrap_or(crate::settings::DEFAULT_CACHE_TTL);
        self.pkgcache_max_size = budgets.pkg;
        self.pkgcache_ttl = settings.duration(keys::PKGCACHE_TTL).unwrap_or(0);
        self.chroot_refresh_interval = settings
            .duration(keys::CHROOT_REFRESH_INTERVAL)
            .unwrap_or(crate::settings::DEFAULT_CHROOT_REFRESH_INTERVAL);
        self.build_limits = limits_from_settings(
            settings,
            keys::BUILD_MEMORY_MAX,
            keys::BUILD_SWAP_MAX,
            keys::BUILD_CPUS,
        );
        self.total_build_limits = limits_from_settings(
            settings,
            keys::TOTAL_BUILD_MEMORY_MAX,
            keys::TOTAL_BUILD_SWAP_MAX,
            keys::TOTAL_BUILD_CPUS,
        );
        self.build_disk_max = settings
            .size(keys::BUILD_DISK_MAX)
            .unwrap_or(crate::settings::DEFAULT_BUILD_DISK_MAX);
        self.disk_max = settings
            .size(keys::DISK_MAX)
            .unwrap_or(crate::settings::DEFAULT_DISK_MAX);
        self.keep_failed = settings.duration(keys::KEEP_FAILED);
    }
}

/// What backs the pool, from `WORKER_POOL`: unset is an image under the chroot
/// directory; a block device is formatted and mounted; a directory is an
/// existing btrfs mount used as it is.
fn pool_backing(
    pool: Option<PathBuf>,
    reserve: bool,
    chroot_dir: &Path,
) -> aurcache_chroot::Backing {
    use std::os::unix::fs::FileTypeExt;
    match pool {
        None => aurcache_chroot::Backing::Image {
            path: chroot_dir.join("pool.img"),
            reserve,
        },
        Some(path) if std::fs::metadata(&path).is_ok_and(|m| m.file_type().is_block_device()) => {
            aurcache_chroot::Backing::Device(path)
        }
        Some(_) => aurcache_chroot::Backing::Mount,
    }
}

/// Where the pool is mounted: the existing mount itself when `WORKER_POOL`
/// names one, otherwise a directory under the chroot directory.
fn pool_mountpoint(pool: Option<PathBuf>, chroot_dir: &Path) -> PathBuf {
    match pool {
        Some(path) if path.is_dir() => path,
        _ => chroot_dir.join("pool"),
    }
}

/// The subvolume the caches live in, at the pool's top.
pub const CACHE_SUBVOLUME: &str = "cache";

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// The same three limits per build and for all builds together, read the same
/// way from whichever pair of keys names them.
fn limits_from_settings(
    settings: &WorkerSettings,
    memory_key: &str,
    swap_key: &str,
    cpus_key: &str,
) -> crate::cgroup::BuildLimits {
    // `0` is the unlimited that leaving it unset already is, and writing it
    // would give the builds no memory at all.
    let memory_max = settings.size(memory_key).filter(|&bytes| bytes > 0);
    crate::cgroup::BuildLimits {
        memory_max,
        // A memory limit means no swap beyond it unless swap is asked for; see
        // `BuildLimits::swap_max` for why it cannot simply be left alone.
        swap_max: settings.size(swap_key).or_else(|| memory_max.map(|_| 0)),
        // A non-positive count is the unlimited that unset is: zero CPUs is not
        // a limit anyone means, and the declared minimum already turns a
        // negative one into a reported rejection.
        cpus: settings
            .float(cpus_key)
            .filter(|&cpus| cpus.is_finite() && cpus > 0.0),
    }
}

/// Resolve the source and package cache budgets.
///
/// `WORKER_CACHE_MAX_SIZE` is the **total** disk the worker's caches may use,
/// split evenly between sources and packages — one number is what most
/// deployments actually want to think about, and the name reads as a total.
///
/// Either pool can be pinned with `WORKER_SRCCACHE_MAX_SIZE` /
/// `WORKER_PKGCACHE_MAX_SIZE`, which also makes the split ratio configurable
/// without a separate knob for it. A pinned pool wins outright: the total is a
/// default for whatever is left unset, not a cap enforced over explicit values,
/// because silently shrinking a number the operator wrote down is worse than
/// exceeding one they did not.
#[must_use]
pub fn split_cache_budgets(total: Option<u64>, src: Option<u64>, pkg: Option<u64>) -> CacheBudgets {
    let half = total.unwrap_or(crate::settings::DEFAULT_TOTAL_CACHE_SIZE) / 2;
    CacheBudgets {
        src: src.unwrap_or(half),
        pkg: pkg.unwrap_or(half),
    }
}

/// Byte budgets of the two cache pools; `0` disables one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheBudgets {
    /// `SRCDEST`, the sources.
    pub src: u64,
    /// The shared pacman package cache.
    pub pkg: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budgets(src: u64, pkg: u64) -> CacheBudgets {
        CacheBudgets { src, pkg }
    }

    #[test]
    fn total_budget_is_split_evenly() {
        assert_eq!(
            split_cache_budgets(Some(1000), None, None),
            budgets(500, 500)
        );
    }

    /// A pinned pool is honoured exactly; the total only fills in the pool the
    /// operator left unset.
    #[test]
    fn a_pinned_pool_wins_and_leaves_the_other_alone() {
        assert_eq!(
            split_cache_budgets(Some(1000), None, Some(900)),
            budgets(500, 900)
        );
        assert_eq!(
            split_cache_budgets(Some(1000), Some(900), None),
            budgets(900, 500)
        );
        assert_eq!(
            split_cache_budgets(Some(10), Some(1), Some(2)),
            budgets(1, 2)
        );
    }

    #[test]
    fn defaults_split_the_default_total() {
        let CacheBudgets { src, pkg } = split_cache_budgets(None, None, None);
        assert_eq!(src, pkg);
        assert_eq!(src + pkg, crate::settings::DEFAULT_TOTAL_CACHE_SIZE);
    }

    /// The values every field had before they were resolved through the
    /// declared settings table, so a key wired to the wrong field is caught
    /// here rather than on a worker.
    ///
    /// Reads the process environment, as the worker does, so it asserts nothing
    /// on a machine that has configured any of these -- which is the same
    /// reason it is worth having: those variables are exactly what this
    /// resolves.
    #[test]
    fn a_clean_environment_still_produces_the_documented_defaults() {
        let configured: Vec<_> = crate::settings::chroot_settings()
            .into_iter()
            .chain(aurcache_worker_core::settings::protocol_settings())
            .map(|spec| spec.env_var)
            .filter(|var| {
                std::env::var_os(var).is_some()
                    || std::env::var_os(format!("{var}_DEFAULT")).is_some()
            })
            .collect();
        if !configured.is_empty() {
            eprintln!("skipped: {configured:?} set in this environment");
            return;
        }

        let cfg = Config::from_env();
        assert_eq!(cfg.core.concurrency, 1);
        assert_eq!(cfg.core.priority, 0);
        assert!(cfg.core.packages.is_empty());
        assert_eq!(cfg.core.poll_interval, 10);
        assert_eq!(cfg.core.build_timeout, 3 * 60 * 60);
        assert_eq!(cfg.core.builddir_max_bytes, 200 * 1024 * 1024 * 1024);
        assert_eq!(cfg.core.builddir_min_free, 50 * 1024 * 1024 * 1024);
        assert_eq!(cfg.keyserver, "hkps://keyserver.ubuntu.com");
        assert_eq!(cfg.cache_max_size, 10 * 1024 * 1024 * 1024);
        assert_eq!(cfg.pkgcache_max_size, 10 * 1024 * 1024 * 1024);
        assert_eq!(cfg.cache_ttl, 30 * 24 * 60 * 60);
        assert_eq!(cfg.pkgcache_ttl, 0);
        assert_eq!(cfg.chroot_refresh_interval, 15 * 60);
        assert_eq!(cfg.build_limits.memory_max, None);
        assert_eq!(cfg.build_limits.swap_max, None);
        assert_eq!(cfg.build_limits.cpus, None);
        assert_eq!(cfg.total_build_limits.memory_max, None);
        assert_eq!(cfg.build_disk_max, 50 * 1024 * 1024 * 1024);
        assert_eq!(cfg.disk_max, 200 * 1024 * 1024 * 1024);
        assert_eq!(cfg.keep_failed, None);
    }

    /// Nothing secret lives in the storage pool, so nothing secret can
    /// outlive a build in a kept one (`kept-<id>*`): the build credential,
    /// the key the worker generates and the agent's socket are all under the
    /// data directory, beside the pool rather than inside it. The agent's
    /// socket reaches a build only as a bind mount, which ends with the
    /// container.
    #[test]
    fn no_secret_lives_in_the_pool() {
        let data_dir = Path::new("/var/lib/aurcache-worker");
        let chroot_dir = data_dir.join("chroot");
        let pool = pool_mountpoint(None, &chroot_dir);
        for secret in [
            crate::credentials::secrets_dir(data_dir),
            crate::credentials::generated_key(data_dir),
            crate::agent::dir(data_dir),
        ] {
            assert!(
                !secret.starts_with(&pool),
                "{} is inside the pool at {}",
                secret.display(),
                pool.display()
            );
        }
    }

    /// Unset, the pool is an image under the chroot directory, sparse unless
    /// reserving; a directory is an existing mount, used where it is.
    #[test]
    fn the_pool_backing_follows_what_worker_pool_names() {
        let chroot = Path::new("/var/lib/aurcache-worker/chroot");
        assert_eq!(
            pool_backing(None, false, chroot),
            aurcache_chroot::Backing::Image {
                path: chroot.join("pool.img"),
                reserve: false
            }
        );
        assert_eq!(
            pool_backing(None, true, chroot),
            aurcache_chroot::Backing::Image {
                path: chroot.join("pool.img"),
                reserve: true
            }
        );
        assert_eq!(pool_mountpoint(None, chroot), chroot.join("pool"));

        let mount = tempfile::tempdir().unwrap();
        assert_eq!(
            pool_backing(Some(mount.path().to_path_buf()), false, chroot),
            aurcache_chroot::Backing::Mount
        );
        assert_eq!(
            pool_mountpoint(Some(mount.path().to_path_buf()), chroot),
            mount.path()
        );
    }

    #[test]
    fn a_users_ids_are_looked_up_by_name() {
        assert_eq!(
            aurcache_chroot::Owner::of_user("root"),
            Some(aurcache_chroot::Owner { uid: 0, gid: 0 })
        );
        assert_eq!(
            aurcache_chroot::Owner::of_user("no-such-user-aurcache-test"),
            None
        );
    }

    #[test]
    fn reserving_takes_the_usual_spellings_of_yes() {
        for yes in ["1", "true", "YES", " on "] {
            assert!(truthy(yes), "{yes:?}");
        }
        for no in ["0", "false", "no", ""] {
            assert!(!truthy(no), "{no:?}");
        }
    }

    /// `0` disables a budget, and must survive as `0` rather than being
    /// mistaken for "unset" and replaced by a default.
    #[test]
    fn zero_disables_rather_than_defaulting() {
        assert_eq!(
            split_cache_budgets(Some(1000), Some(0), None),
            budgets(0, 500)
        );
        assert_eq!(split_cache_budgets(Some(0), None, None), budgets(0, 0));
    }
}
