//! What a worker asks the agent to build: the job, as data.
//!
//! Written by the worker as `<work>/in/spec.json` and read by the agent before
//! anything else. Within one [`INTERFACE`] these types only gain optional
//! fields; anything else is the next interface.

use aurcache_common::worker::JobVcsSource;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What the agent and a worker must agree on to work together.
pub const INTERFACE: u32 = 2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildSpec {
    /// `INTERFACE` of whoever wrote this; the agent refuses any other.
    pub interface: u32,
    pub pkgbase: String,
    /// Arguments added to makepkg's command line, as the job gave them.
    pub build_flags: Vec<String>,
    /// The job's makepkg assignments. Where things go and how many jobs run
    /// are the agent's: it drops such lines and sets its own after these.
    pub makepkg_conf: String,
    /// Whole, with the AURCache repository already appended by the worker.
    pub pacman_conf: String,
    pub mirrorlist: Option<String>,
    pub pgp_keys: Vec<String>,
    pub keyserver: String,
    pub vcs_sources: Vec<JobVcsSource>,
    /// The CPUs the build is limited to, which size its parallelism.
    pub cpus: Option<f64>,
    /// Where the build's own trees go, inside the container.
    pub scratch: PathBuf,
    /// The worker's SSH key, for the build to fetch its sources with.
    pub ssh: Option<SshAccess>,
}

/// The use of an SSH key the worker holds: the build asks the worker for
/// signatures over the link ([`crate::ssh_link`]) while makepkg downloads
/// the sources, and never sees the key itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SshAccess {
    /// The hosts' keys, as a `known_hosts` file holds them. The build's ssh
    /// accepts no other host.
    pub known_hosts: String,
}
