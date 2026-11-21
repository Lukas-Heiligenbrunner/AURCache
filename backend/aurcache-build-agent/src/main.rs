//! `aurcache-build-agent`: runs a build inside its container, and is the other
//! end of every exchange a worker has with that container.
//!
//! This file is the command line and nothing else: each subcommand is one
//! function in [`agent`]. `build` (and the two subcommands it runs itself,
//! `pacman-request` and `unpack`) is in [`agent::build`]; the subcommand a
//! worker runs through `exec` is in [`agent::transfer`].

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
    /// The client `PACMAN_AUTH` runs: asks root to install what makepkg
    /// asked for, and exits with pacman's status (126 when refused).
    #[command(disable_help_flag = true, disable_version_flag = true)]
    PacmanRequest {
        /// What makepkg ran after `PACMAN_AUTH`, pacman's path first.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        argv: Vec<String>,
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
        Command::PacmanRequest { argv } => agent::build::pacman_request(argv),
        Command::Unpack { archive, into } => agent::build::unpack(&archive, &into),
        Command::SshLink { socket } => agent::transfer::ssh_link(&socket),
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::{CommandFactory, Parser};

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    /// makepkg's words reach the helper as they were, flags included.
    #[test]
    fn pacman_request_takes_every_word() {
        let cli = Cli::try_parse_from([
            "aurcache-build-agent",
            "pacman-request",
            "/usr/bin/pacman",
            "--noconfirm",
            "-S",
            "--asdeps",
            "--help",
            "tree>=2",
        ])
        .unwrap();
        let Some(Command::PacmanRequest { argv }) = cli.command else {
            panic!("not pacman-request");
        };
        assert_eq!(
            argv,
            [
                "/usr/bin/pacman",
                "--noconfirm",
                "-S",
                "--asdeps",
                "--help",
                "tree>=2"
            ]
        );
    }

    #[test]
    fn interface_stands_alone() {
        assert!(Cli::try_parse_from(["aurcache-build-agent", "--interface"]).is_ok());
        assert!(
            Cli::try_parse_from(["aurcache-build-agent", "--interface", "pacman-request"]).is_err()
        );
    }
}
