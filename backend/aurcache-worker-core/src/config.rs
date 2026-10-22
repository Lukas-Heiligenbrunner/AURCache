//! Protocol-level worker configuration, assembled from environment variables
//! with zero-config defaults so the bundled single-host topology needs no
//! tuning.
//!
//! Only settings the *protocol* needs live here — identity, enrollment,
//! scheduling and liveness. How a job is actually turned into a package is an
//! executor concern, so each executor crate parses its own settings alongside
//! this (see `aurcache_worker::config::Config`).

use crate::settings::{WorkerSettings, keys, protocol_settings};
use aurcache_common::units::parse_duration;
use std::path::PathBuf;

/// Worker configuration shared by every executor.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// Backend API base URL, e.g. `https://aurcache:8080`.
    pub aurcache_url: String,
    /// Optional pinned CA/server fingerprint (SHA-256 hex). When unset the
    /// worker trusts-on-first-use the CA it downloads during enrollment.
    pub server_ca_fingerprint: Option<String>,
    /// Shared enrollment volume (bundled topology drops the CSR here).
    pub enrollment_dir: Option<PathBuf>,
    /// Shared-secret enrollment token (fallback path).
    pub enrollment_token: Option<String>,
    /// Architectures this worker builds natively.
    pub native_arches: Vec<String>,
    /// Architectures this worker can build via emulation.
    pub emulated_arches: Vec<String>,
    /// Exact pkgbase names this worker is specially provisioned for
    /// (credentials, licensed toolchain, scratch space). Packages named here
    /// are reserved to workers that name them.
    pub packages: Vec<String>,
    /// Scheduling preference; higher wins. Lower-priority workers hold back
    /// while a higher-priority one has capacity.
    pub priority: i32,
    /// Maximum concurrent builds.
    pub concurrency: usize,
    /// Human-friendly worker name (defaults to hostname).
    pub name: String,
    /// Directory where identity (key + cert) is persisted.
    pub data_dir: PathBuf,
    /// Heartbeat cadence in seconds.
    pub heartbeat_interval: u64,
    /// Lease TTL in seconds; the worker self-aborts a build it cannot report
    /// for longer than this (the server will have abandoned it).
    pub lease_ttl: u64,
    /// Poll interval when no job is available, in seconds.
    pub poll_interval: u64,
    /// Per-build timeout in seconds (`0` disables the worker-side timeout;
    /// the build is killed and reported as a timeout when exceeded).
    pub build_timeout: u64,
    /// Total bytes the persistent build cache may occupy.
    pub builddir_max_bytes: u64,
    /// Bytes to keep free on the persistent build-tree filesystem.
    pub builddir_min_free: u64,
    /// Overrides the host used in the `[repo]` section, for deployments where
    /// the address used to reach the worker protocol is not the address that
    /// serves the package repository.
    pub repo_host: Option<String>,
    /// Overrides the whole repository base URL, not just its host.
    ///
    /// The server renders one template for every worker, so scheme, port and
    /// path are the same for all of them and only the host varies. That holds
    /// while every worker reaches the repository the same way, and stops
    /// holding as soon as one does not: a worker inside the compose network
    /// uses `http://aurcache:8081` while one across the internet comes in
    /// through a reverse proxy at `https://aur.example.com/repo`. Differing in
    /// host alone cannot express that.
    ///
    /// Takes precedence over [`Self::repo_host`], which is the narrower case of
    /// the same thing.
    pub repo_url: Option<String>,
    /// A mirrorlist this worker uses in place of anything the server sends.
    ///
    /// From `WORKER_MIRRORLIST_SERVERS` (a `;`-separated server list, rendered
    /// into `Server =` lines) or `WORKER_MIRRORLIST_FILE` (a path to a ready
    /// mirrorlist). Set for a worker whose local mirrors beat the server's --
    /// a worker on other hardware, or on the far side of a slow link from the
    /// mirror the server happens to prefer.
    ///
    /// When this is set the server is told not to send one at all, rather than
    /// sending bytes the worker would discard.
    pub mirrorlist: Option<String>,
    /// The settings this worker declares, resolved against its environment.
    ///
    /// Held rather than consumed and discarded: it is what registration reports
    /// as this worker's configurable surface and what the heartbeat reports it
    /// is running, and both have to say the same thing as the fields above,
    /// which read their values from it.
    ///
    /// An executor extends it with its own table
    /// ([`WorkerSettings::extended`](crate::settings::WorkerSettings::extended)).
    pub settings: WorkerSettings,
}

/// Read an environment variable, treating blank values as unset.
///
/// The trimmed value: returning the raw string would keep a leading space
/// that later parsing (e.g. URL handling, which only trims trailing slashes)
/// does not remove.
#[must_use]
pub fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Read and parse an environment variable, treating blank values as unset.
///
/// A value that is set but does not parse is reported and then treated as
/// unset, so the caller's default applies. Falling back is still right -- a
/// typo should not keep a worker from starting -- but doing it silently is
/// not: `WORKER_BUILDDIR_MAX_BYTES=450G` used to mean the 200 GiB default
/// with nothing to say so.
#[must_use]
pub fn env_parse<T>(key: &str) -> Option<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = env_opt(key)?;
    match raw.trim().parse() {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!("ignoring {key}={raw:?} ({e}); using the default");
            None
        }
    }
}

/// Read a duration in seconds from the environment, as [`parse_duration`]
/// reads one.
///
/// Like [`env_parse`], a value that does not parse is reported and treated as
/// unset.
#[must_use]
pub fn env_duration(key: &str) -> Option<u64> {
    let raw = env_opt(key)?;
    let seconds = parse_duration(&raw);
    if seconds.is_none() {
        tracing::warn!(
            "ignoring {key}={raw:?} (expected seconds, or a duration such as 90s, 15m, 3h, \
             1h30m or 30d); using the default"
        );
    }
    seconds
}

/// Split a comma- or space-separated list (arches, pkgbases), dropping blanks.
#[must_use]
pub fn parse_list(raw: &str) -> Vec<String> {
    raw.split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Detect the host architecture via `uname -m`, defaulting to `x86_64`.
#[must_use]
pub fn detect_arch() -> String {
    std::process::Command::new("uname")
        .arg("-m")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "x86_64".to_string())
}

fn detect_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "aurcache-worker".to_string())
}

impl CoreConfig {
    /// Build a [`CoreConfig`] from the process environment, applying
    /// zero-config defaults for everything not explicitly set.
    #[must_use]
    pub fn from_env() -> Self {
        let native_arches = env_opt("WORKER_ARCHES")
            .map(|s| parse_list(&s))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![detect_arch()]);

        let emulated_arches = env_opt("WORKER_EMULATED_ARCHES")
            .map(|s| parse_list(&s))
            .unwrap_or_default();

        let settings = WorkerSettings::from_env(protocol_settings());

        let mut cfg = Self {
            aurcache_url: env_opt("AURCACHE_URL")
                .unwrap_or_else(|| "https://localhost:8080".to_string())
                .trim_end_matches('/')
                .to_string(),
            server_ca_fingerprint: env_opt("AURCACHE_SERVER_CA_FINGERPRINT")
                .map(|s| aurcache_ca::normalize_fingerprint(&s)),
            enrollment_dir: env_opt("AURCACHE_ENROLLMENT_DIR")
                .map(PathBuf::from)
                .or_else(|| Some(PathBuf::from("/enroll"))),
            enrollment_token: env_opt("AURCACHE_ENROLLMENT_TOKEN"),
            native_arches,
            emulated_arches,
            name: env_opt("WORKER_NAME").unwrap_or_else(detect_hostname),
            data_dir: env_opt("WORKER_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/var/lib/aurcache-worker")),
            heartbeat_interval: env_duration("WORKER_HEARTBEAT_INTERVAL").unwrap_or(15),
            lease_ttl: env_duration("LEASE_TTL").unwrap_or(60),
            repo_host: env_opt("AURCACHE_REPO_HOST"),
            repo_url: env_opt("AURCACHE_REPO_URL"),
            mirrorlist: local_mirrorlist(),
            // Filled in from `settings` just below, by the same code that
            // fills them in again whenever the server delivers new values.
            packages: Vec::new(),
            priority: 0,
            concurrency: 1,
            poll_interval: crate::settings::DEFAULT_POLL_INTERVAL,
            build_timeout: crate::settings::DEFAULT_BUILD_TIMEOUT,
            builddir_max_bytes: crate::settings::DEFAULT_BUILDDIR_MAX_BYTES,
            builddir_min_free: crate::settings::DEFAULT_BUILDDIR_MIN_FREE,
            settings,
        };
        cfg.read_settings();
        cfg
    }

    /// The same configuration with `settings` in force: every field that is
    /// a declared setting read again from them, and nothing else touched.
    ///
    /// What the server delivering new values comes down to. Identity, the
    /// enrollment settings and everything else the server may not set stay
    /// exactly as the machine configured them.
    #[must_use]
    pub fn with_settings(&self, settings: WorkerSettings) -> Self {
        let mut cfg = Self {
            settings,
            ..self.clone()
        };
        cfg.read_settings();
        cfg
    }

    /// Read every declared field from [`Self::settings`].
    fn read_settings(&mut self) {
        let settings = &self.settings;
        // Clamped rather than refused: a worker that ran no builds at all would
        // be a machine silently doing nothing, and the declared minimum of 1 is
        // what the value is checked against before it ever gets here.
        self.concurrency = usize::try_from(
            settings
                .integer(keys::CONCURRENCY)
                .unwrap_or(crate::settings::DEFAULT_CONCURRENCY),
        )
        .unwrap_or(1)
        .max(1);
        self.packages = settings.list(keys::PACKAGES);
        // A malformed value degrades to the neutral default, never to maximal
        // scheduling privilege: an out-of-range typo must not outrank the
        // whole fleet.
        self.priority = i32::try_from(settings.integer(keys::PRIORITY).unwrap_or(0)).unwrap_or(0);
        self.poll_interval = settings
            .duration(keys::POLL_INTERVAL)
            .unwrap_or(crate::settings::DEFAULT_POLL_INTERVAL);
        // What bounds the persistent build cache. A cap rather than only a
        // free-space floor, because a floor does nothing on a large pool: trees
        // would grow into the terabytes before it ever triggered.
        self.builddir_max_bytes = settings
            .size(keys::BUILDDIR_MAX_BYTES)
            .unwrap_or(crate::settings::DEFAULT_BUILDDIR_MAX_BYTES);
        self.builddir_min_free = settings
            .size(keys::BUILDDIR_MIN_FREE)
            .unwrap_or(crate::settings::DEFAULT_BUILDDIR_MIN_FREE);
        self.build_timeout = settings
            .duration(keys::BUILD_TIMEOUT)
            .unwrap_or(crate::settings::DEFAULT_BUILD_TIMEOUT);
    }
}

/// The worker's own mirrorlist, if it has been given one.
///
/// `WORKER_MIRRORLIST_SERVERS` wins over `WORKER_MIRRORLIST_FILE`: it is the
/// more specific statement of intent, and an unreadable file should not
/// silently override an explicit list. A file that cannot be read is a warning
/// and not a failure -- the worker then takes the server's mirrorlist, which is
/// what it would have done with neither variable set.
fn local_mirrorlist() -> Option<String> {
    if let Some(servers) = env_opt("WORKER_MIRRORLIST_SERVERS") {
        let rendered: String = servers
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("Server = {s}\n"))
            .collect();
        if !rendered.is_empty() {
            return Some(rendered);
        }
    }
    let path = env_opt("WORKER_MIRRORLIST_FILE")?;
    match std::fs::read_to_string(&path) {
        Ok(content) if !content.trim().is_empty() => Some(content),
        Ok(_) => {
            tracing::warn!("WORKER_MIRRORLIST_FILE {path} is empty; using the server's mirrorlist");
            None
        }
        Err(e) => {
            tracing::warn!("WORKER_MIRRORLIST_FILE {path} unreadable ({e}); using the server's");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_arch_lists() {
        assert_eq!(parse_list("x86_64"), vec!["x86_64"]);
        assert_eq!(parse_list("aarch64, armv7h"), vec!["aarch64", "armv7h"]);
        assert_eq!(parse_list("a b,c  d"), vec!["a", "b", "c", "d"]);
        assert!(parse_list("  ,  ").is_empty());
    }
}
