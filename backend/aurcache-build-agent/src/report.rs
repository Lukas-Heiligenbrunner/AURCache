//! What the agent hands back: how the build ended and what it measured, and,
//! for a worker that reaches the container only through `exec`, the packages
//! it left.
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
    pub peak_memory_bytes: Option<u64>,
    pub oom_kills: Option<u64>,
    /// Bytes under the work and scratch directories when the build ended.
    pub disk_bytes: Option<u64>,
    /// Keyed by `JobVcsSource::source_url`.
    pub vcs_commits: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum Ended {
    /// makepkg ran, and exited with this code.
    Exited { code: i32 },
    /// A signal ended makepkg.
    Signaled { signal: i32 },
    /// A step before makepkg failed, so the package was never built.
    Setup { step: SetupStep, message: String },
}

/// The steps before makepkg, each of which can keep a build from starting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStep {
    /// Reading or accepting the spec.
    Spec,
    /// Checking and writing `pacman.conf`, the mirrorlist, `makepkg.conf`.
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
    /// Loading the SSH credential into the build user's agent.
    Credential,
}

/// What `manifest` prints.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Handback {
    pub interface: u32,
    /// `None` when the build container ended without writing one.
    pub report: Option<BuildReport>,
    pub artifacts: Vec<Artifact>,
}

/// One package in `<work>/pkg/`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
}
