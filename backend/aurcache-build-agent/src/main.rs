//! `aurcache-build-agent`: runs a build inside its container, as its PID 1.
//!
//! This file is the command line and nothing else: `build` is in
//! [`agent::build`], and `ssh-link`, which a worker execs into the
//! container, in [`agent::ssh_link`].

mod agent;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = aurcache_build_agent::layout::BIN_NAME, version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the build described by `<work>/in`, as the container's PID 1.
    Build {
        #[arg(long)]
        work: PathBuf,
    },
    /// Carry the SSH link: connect to the agent's link socket and copy
    /// between it and stdin and stdout.
    #[command(hide = true)]
    SshLink {
        #[arg(long)]
        socket: PathBuf,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Build { work } => agent::build::run(&work),
        Command::SshLink { socket } => agent::ssh_link::run(&socket),
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }
}
