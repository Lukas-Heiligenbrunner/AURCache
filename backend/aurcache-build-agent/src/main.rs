//! `aurcache-build-agent`: runs a build inside its container, and is the other
//! end of every exchange a worker has with that container.
//!
//! This file is the command line and nothing else: each subcommand is one
//! function in [`agent`]. `build` (and the subcommand it runs itself,
//! `unpack`) is in [`agent::build`]; the subcommand a worker runs through
//! `exec` is in [`agent::transfer`].

mod agent;

use aurcache_build_agent::spec::INTERFACE;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = aurcache_build_agent::layout::BIN_NAME,
    version,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Print the interface number this agent speaks, and exit.
    #[arg(long)]
    interface: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the build described by `<work>/in`, as the container's PID 1.
    Build {
        #[arg(long)]
        work: PathBuf,
        /// Also write the build's output to this file.
        #[arg(long)]
        log_file: Option<PathBuf>,
    },
    /// Internal, run by `build` as the build user: extract the source archive.
    #[command(hide = true)]
    Unpack {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        into: PathBuf,
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
    let cli = Cli::parse();
    if cli.interface {
        println!("{INTERFACE}");
        return ExitCode::SUCCESS;
    }
    let Some(command) = cli.command else {
        eprintln!("a subcommand is required; see --help");
        return ExitCode::from(2);
    };
    match command {
        Command::Build { work, log_file } => agent::build::run(&work, log_file.as_deref()),
        Command::Unpack { archive, into } => agent::build::unpack(&archive, &into),
        Command::SshLink { socket } => agent::transfer::ssh_link(&socket),
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::{CommandFactory, Parser};

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn interface_stands_alone() {
        assert!(Cli::try_parse_from(["aurcache-build-agent", "--interface"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "aurcache-build-agent",
                "--interface",
                "build",
                "--work",
                "/w"
            ])
            .is_err()
        );
    }
}
