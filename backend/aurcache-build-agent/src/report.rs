//! What the agent hands back: how the build ended.
//!
//! Within one [`INTERFACE`](crate::spec::INTERFACE) these types only gain
//! optional fields; anything else is the next interface.

use serde::{Deserialize, Serialize};

/// `<work>/out/report.json`, written last, once nothing the build started is
/// still running.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildReport {
    pub interface: u32,
    pub ended: Ended,
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
}
