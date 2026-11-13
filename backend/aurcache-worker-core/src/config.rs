//! Protocol-level worker configuration, assembled from environment variables
//! with zero-config defaults so the bundled single-host topology needs no
//! tuning.
//!
//! Only settings the *protocol* needs live here — identity, enrollment,
//! scheduling and liveness. How a job is actually turned into a package is an
//! executor concern, so each executor crate parses its own settings alongside
//! this (see `aurcache_worker::config::Config`).

use crate::settings::{SettingSpec, WorkerSettings, keys, protocol_settings};
use aurcache_common::units::parse_duration;
use std::path::PathBuf;
use std::time::Duration;

/// Worker configuration shared by every executor.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// The server's worker-protocol URL, e.g. `https://aurcache:8083`.
    pub aurcache_url: String,
    /// Optional pinned CA/server fingerprint (SHA-256 hex). When unset the
    /// worker trusts-on-first-use the CA it downloads during enrollment.
    pub server_ca_fingerprint: Option<String>,
    /// Shared enrollment volume (bundled topology drops the CSR here).
    pub enrollment_dir: PathBuf,
    /// Shared-secret enrollment token (fallback path).
    pub enrollment_token: Option<String>,
    /// The secret proving this is the hybrid image's embedded worker; see
    /// [`aurcache_common::worker::RegisterRequest::embedded_secret`].
    pub embedded_secret: Option<String>,
    /// Architectures this worker builds (`WORKER_ARCHES`, or the host's).
    ///
    /// All the protocol carries: how they are built is the executor's
    /// business, so an executor that can build more than the host's -- the
    /// container one, through binfmt -- adds them from its own settings.
    pub arches: Vec<String>,
    /// Human-friendly worker name (defaults to hostname).
    pub name: String,
    /// The version this worker reports: its binary's own, not this crate's,
    /// since every executor is released on its own.
    pub version: String,
    /// Directory where identity (key + cert) is persisted.
    pub data_dir: PathBuf,
    /// Heartbeat cadence.
    pub heartbeat_interval: Duration,
    /// Lease TTL; the worker self-aborts a build it cannot report for longer
    /// than this (the server will have abandoned it).
    pub lease_ttl: Duration,
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
    /// Every setting this worker declares -- the protocol's and the
    /// executor's -- resolved against its environment and whatever the server
    /// has delivered.
    ///
    /// Held rather than consumed and discarded: it is what registration reports
    /// as this worker's configurable surface and what the heartbeat reports it
    /// is running, and both have to say the same thing as [`Self::tunables`],
    /// which is read from it.
    pub settings: WorkerSettings,
    /// The protocol's declared settings, as read from [`Self::settings`].
    pub tunables: CoreTunables,
}

/// The protocol settings a worker runs with: scheduling and liveness, read
/// whole from the declared settings and read again whenever the server
/// delivers values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreTunables {
    /// Exact pkgbase names this worker is specially provisioned for
    /// (credentials, licensed toolchain, scratch space). Packages named here
    /// are reserved to workers that name them.
    pub packages: Vec<String>,
    /// Scheduling preference; higher wins. Lower-priority workers hold back
    /// while a higher-priority one has capacity.
    pub priority: i32,
    /// Maximum concurrent builds.
    pub concurrency: usize,
    /// How long to wait before asking again when no job is available.
    pub poll_interval: Duration,
    /// How long one build may run; `None` (a `0` setting) leaves it to the
    /// server's job timeout.
    pub build_timeout: Option<Duration>,
}

impl CoreTunables {
    /// Read every protocol setting from `settings`.
    #[must_use]
    pub fn from_settings(settings: &WorkerSettings) -> Self {
        Self {
            packages: settings.list(keys::PACKAGES),
            // A malformed value degrades to the neutral default, never to
            // maximal scheduling privilege: an out-of-range typo must not
            // outrank the whole fleet.
            priority: i32::try_from(settings.integer(keys::PRIORITY).unwrap_or(0)).unwrap_or(0),
            // Clamped rather than refused: a worker that ran no builds at all
            // would be a machine silently doing nothing, and the declared
            // minimum of 1 is what the value is checked against before it ever
            // gets here.
            concurrency: usize::try_from(
                settings
                    .integer(keys::CONCURRENCY)
                    .unwrap_or(crate::settings::DEFAULT_CONCURRENCY),
            )
            .unwrap_or(1)
            .max(1),
            poll_interval: settings
                .duration(keys::POLL_INTERVAL)
                .unwrap_or(crate::settings::DEFAULT_POLL_INTERVAL),
            build_timeout: Some(
                settings
                    .duration(keys::BUILD_TIMEOUT)
                    .unwrap_or(crate::settings::DEFAULT_BUILD_TIMEOUT),
            )
            .filter(|limit| !limit.is_zero()),
        }
    }
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

/// Read a duration from the environment, as [`parse_duration`] reads one.
///
/// Like [`env_parse`], a value that does not parse is reported and treated as
/// unset.
#[must_use]
pub fn env_duration(key: &str) -> Option<Duration> {
    let raw = env_opt(key)?;
    let seconds = parse_duration(&raw);
    if seconds.is_none() {
        tracing::warn!(
            "ignoring {key}={raw:?} (expected seconds, or a duration such as 90s, 15m, 3h, \
             1h30m or 30d); using the default"
        );
    }
    seconds.map(Duration::from_secs)
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
    ///
    /// `version` is what the worker reports, from its binary:
    /// `full_version(env!("CARGO_PKG_VERSION"))` there. `executor_settings` is
    /// the executor's own table, declared beside the protocol's: registration
    /// reports the two as one, and both are resolved here, once.
    #[must_use]
    pub fn from_env(version: String, executor_settings: Vec<SettingSpec>) -> Self {
        let arches = env_opt("WORKER_ARCHES")
            .map(|s| parse_list(&s))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![detect_arch()]);

        let settings = WorkerSettings::from_env(protocol_settings()).extended(executor_settings);

        Self {
            aurcache_url: env_opt("AURCACHE_URL")
                .unwrap_or_else(|| "https://localhost:8083".to_string())
                .trim_end_matches('/')
                .to_string(),
            server_ca_fingerprint: env_opt("AURCACHE_SERVER_CA_FINGERPRINT")
                .map(|s| aurcache_ca::normalize_fingerprint(&s)),
            enrollment_dir: env_opt("AURCACHE_ENROLLMENT_DIR")
                .map_or_else(|| PathBuf::from("/enroll"), PathBuf::from),
            enrollment_token: env_opt("AURCACHE_ENROLLMENT_TOKEN"),
            embedded_secret: env_opt("AURCACHE_EMBEDDED_WORKER_SECRET"),
            arches,
            name: env_opt("WORKER_NAME").unwrap_or_else(detect_hostname),
            version,
            data_dir: env_opt("WORKER_DATA_DIR")
                .map_or_else(|| PathBuf::from("/var/lib/aurcache-worker"), PathBuf::from),
            heartbeat_interval: env_duration("WORKER_HEARTBEAT_INTERVAL")
                .unwrap_or(Duration::from_secs(15)),
            lease_ttl: env_duration("LEASE_TTL").unwrap_or(Duration::from_secs(60)),
            repo_host: env_opt("AURCACHE_REPO_HOST"),
            repo_url: env_opt("AURCACHE_REPO_URL"),
            mirrorlist: local_mirrorlist(),
            tunables: CoreTunables::from_settings(&settings),
            settings,
        }
    }

    /// The same configuration with `settings` in force: the tunables read
    /// again from them, and nothing else touched.
    ///
    /// What the server delivering new values comes down to. Identity, the
    /// enrollment settings and everything else the server may not set stay
    /// exactly as the machine configured them.
    #[must_use]
    pub fn with_settings(&self, settings: WorkerSettings) -> Self {
        Self {
            tunables: CoreTunables::from_settings(&settings),
            settings,
            ..self.clone()
        }
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
