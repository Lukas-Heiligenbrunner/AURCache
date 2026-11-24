//! `aurcache-build-agent`: runs a build inside its container, as its PID 1.
//!
//! This file is the command line and nothing else: `build` is in
//! [`agent::build`], `ssh-link`, which a worker execs into the container, in
//! [`agent::ssh_link`], and the subcommands of the Kubernetes worker's pods
//! in [`agent::transfer`].

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
        /// Write the build log to this file instead of stdout.
        #[arg(long)]
        log_file: Option<PathBuf>,
    },
    /// Install this binary into `<tools>`, then wait for the job to arrive.
    Stage {
        #[arg(long)]
        work: PathBuf,
        #[arg(long)]
        tools: PathBuf,
    },
    /// Take the job from stdin into `<work>/in`.
    Receive {
        #[arg(long)]
        work: PathBuf,
    },
    /// Keep the container up until it is told to stop.
    Hold,
    /// Write the build log from an offset, following it until the build ends.
    Tail {
        #[arg(long)]
        work: PathBuf,
        #[arg(long)]
        from: u64,
    },
    /// Print the report and the packages' names and sizes, as JSON.
    Manifest {
        #[arg(long)]
        work: PathBuf,
    },
    /// Write one package's bytes to stdout.
    Send {
        #[arg(long)]
        work: PathBuf,
        #[arg(long)]
        name: String,
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
        Command::Build { work, log_file } => agent::build::run(&work, log_file.as_deref()),
        Command::Stage { work, tools } => agent::transfer::stage(&work, &tools),
        Command::Receive { work } => agent::transfer::receive(&work),
        Command::Hold => agent::transfer::hold(),
        Command::Tail { work, from } => agent::transfer::tail(&work, from),
        Command::Manifest { work } => agent::transfer::manifest(&work),
        Command::Send { work, name } => agent::transfer::send(&work, &name),
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
