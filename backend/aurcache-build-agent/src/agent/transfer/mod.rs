//! The other end of an `exec` a worker makes into a build container: carrying
//! the SSH link.
//!
//! The subcommand exits 0 when it did what it was asked and 1 otherwise,
//! saying why on stderr.

mod ssh_link;

use aurcache_build_agent::layout::BIN_NAME;
use std::path::Path;
use std::process::ExitCode;

/// Connect to the agent's link socket and copy bytes between it and
/// stdin and stdout, until either side ends.
pub fn ssh_link(socket: &Path) -> ExitCode {
    finish(ssh_link::run(socket))
}

/// Turn what a subcommand did into its exit status, naming the error on stderr.
fn finish(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{BIN_NAME}: {error:#}");
            ExitCode::FAILURE
        }
    }
}
