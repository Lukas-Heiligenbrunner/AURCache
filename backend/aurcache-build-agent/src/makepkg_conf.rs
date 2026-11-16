//! The `makepkg.conf` a build runs with.
//!
//! The image's own, then the job's assignments without the lines that say
//! where things go or how many jobs run, then the agent's: `PKGDEST`,
//! `BUILDDIR`, `SRCDEST`, the parallelism for the build's CPUs, `PACMAN_AUTH`
//! on the helper and, with a credential, the ssh-agent's socket.

use crate::layout::{Scratch, Work};
use std::path::Path;

/// Everything the rendering depends on.
pub struct Rendering<'a> {
    /// The job's makepkg assignments.
    pub job: &'a str,
    pub work: &'a Work,
    pub scratch: &'a Scratch,
    pub cpus: Option<f64>,
    /// The agent's own path inside the container, for `PACMAN_AUTH`.
    pub agent: &'a Path,
    pub ssh: Option<Ssh<'a>>,
}

/// The build user's ssh-agent, for a build with a credential.
pub struct Ssh<'a> {
    pub socket: &'a Path,
    pub known_hosts: Option<&'a Path>,
}

/// `source /etc/makepkg.conf`, the job's lines minus the layout ones, then the agent's.
#[must_use]
pub fn render(rendering: &Rendering<'_>) -> String {
    // WP1 writes this.
    let _ = rendering;
    unimplemented!()
}
