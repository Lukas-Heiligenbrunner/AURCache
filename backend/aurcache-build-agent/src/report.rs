//! What the agent hands back: how the build ended and what it measured.
//!
//! Within one [`INTERFACE`](crate::spec::INTERFACE) these types only gain
//! optional fields; anything else is the next interface.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `<work>/out/report.json`, written last, once nothing the build started is
/// still running.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildReport {
    pub interface: u32,
    pub ended: Ended,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom_kills: Option<u64>,
    /// Bytes under the work and scratch directories when the build ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    /// Keyed by `JobVcsSource::source_url`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vcs_commits: BTreeMap<String, String>,
}

impl BuildReport {
    /// A report of how the build ended, with nothing measured.
    #[must_use]
    pub fn unmeasured(ended: Ended) -> Self {
        Self {
            interface: crate::spec::INTERFACE,
            ended,
            peak_memory_bytes: None,
            oom_kills: None,
            disk_bytes: None,
            vcs_commits: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum Ended {
    /// makepkg ran, and exited with this code.
    Exited { code: i32 },
    /// A signal ended makepkg.
    Signaled { signal: i32 },
    /// A step before makepkg failed, or makepkg could not be started, so the
    /// package was never built.
    Setup { step: SetupStep, message: String },
}

/// The steps before makepkg, each of which can keep a build from starting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStep {
    /// Reading or accepting the spec.
    Spec,
    /// Writing `pacman.conf`, the mirrorlist, `makepkg.conf`.
    Config,
    /// Initialising pacman's keyring.
    Keyring,
    /// Upgrading the image.
    Sync,
    /// Adding the build user and giving it its directories.
    User,
    /// Unpacking the source archive.
    Source,
    /// Importing the job's PGP keys.
    Keys,
    /// Reading the PKGBUILD's dependencies and installing them.
    Dependencies,
    /// Starting makepkg.
    Makepkg,
    /// Taking the worker's SSH link, or giving the build its socket.
    Credential,
}
