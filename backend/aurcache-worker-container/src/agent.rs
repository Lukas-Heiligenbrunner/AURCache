//! Where the agent binary is on the worker's own machine.

use anyhow::Result;
use std::path::PathBuf;

/// The variable naming the agent binary, for a layout the lookup does not
/// cover.
pub const VARIABLE: &str = "WORKER_BUILD_AGENT";
/// Where the worker's package installs the agent.
pub const INSTALLED: &str = "/usr/lib/aurcache/bin/aurcache-build-agent";

/// The agent binary on this machine: `WORKER_BUILD_AGENT`, else beside the
/// running executable, else `INSTALLED`.
pub fn locate() -> Result<PathBuf> {
    // WP3 writes this.
    unimplemented!()
}
