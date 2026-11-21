//! What a worker asks the agent to build: the job, as data.
//!
//! Written by the worker as `<work>/in/spec.json` and read by the agent before
//! anything else. Within one [`INTERFACE`] these types only gain optional
//! fields; anything else is the next interface.

use aurcache_common::worker::JobVcsSource;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What the agent and a worker must agree on to work together.
pub const INTERFACE: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildSpec {
    /// `INTERFACE` of whoever wrote this; the agent refuses any other.
    pub interface: u32,
    pub pkgbase: String,
    /// The architecture the job is built for, as pacman names it: which of
    /// the PKGBUILD's `depends_<arch>` arrays are installed.
    pub arch: String,
    /// Arguments added to makepkg's command line, as the job gave them.
    pub build_flags: Vec<String>,
    /// The job's makepkg assignments. Where things go and how many jobs run
    /// are the agent's: it drops such lines and sets its own after these.
    pub makepkg_conf: String,
    /// Whole, with the AURCache repository already appended by the worker:
    /// what the databases are synced with and what the build installs its
    /// dependencies with.
    pub pacman_conf: String,
    /// The same without the AURCache repository: what the image is upgraded
    /// with. pacman acts on a package's `replaces=` during an upgrade and at
    /// no other time, so an upgrade that could see AURCache's repository
    /// would let a package built there install itself into every build.
    pub pacman_upgrade_conf: String,
    pub mirrorlist: Option<String>,
    pub pgp_keys: Vec<String>,
    pub keyserver: String,
    pub vcs_sources: Vec<JobVcsSource>,
    /// The CPUs the build is limited to, which size its parallelism.
    pub cpus: Option<f64>,
    /// Where the build's own trees go, inside the container.
    pub scratch: PathBuf,
    /// The worker's SSH key, for the build to fetch its sources with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshAccess>,
}

/// The use of an SSH key the worker holds: the build asks the worker's
/// ssh-agent for signatures over the link ([`crate::ssh_link`]) while it
/// runs, and never sees the key itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SshAccess {
    /// The hosts' keys, as a `known_hosts` file holds them. The ssh git runs
    /// (`GIT_SSH_COMMAND`) accepts no other host; an ssh the build runs
    /// itself reads the image's configuration.
    pub known_hosts: String,
}
