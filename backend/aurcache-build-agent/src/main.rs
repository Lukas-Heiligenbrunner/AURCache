//! `aurcache-build-agent`: runs a build inside its container, and is the other
//! end of every exchange a worker has with that container.
//!
//! This file is the command line and nothing else: each subcommand is one
//! function in [`agent`]. `build` (and the two subcommands it runs itself,
//! `pacman-request` and `unpack`) is in [`agent::build`]; the subcommands a
//! worker that reaches the container only through `exec` uses are in
//! [`agent::transfer`].

mod agent;

use aurcache_build_agent::spec::INTERFACE;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

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
    /// Install this binary into `<tools>`, then wait for the job to arrive.
    Stage {
        #[arg(long)]
        work: PathBuf,
        #[arg(long)]
        tools: PathBuf,
    },
    /// Take the job, as a tar on stdin, into `<work>/in`.
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
    /// Print the report and the packages' names, sizes and digests, as JSON.
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
    /// Try a TCP connection to each target, and print which answered.
    Probe {
        /// Seconds to give each connection.
        #[arg(long)]
        timeout: u64,
        /// A `host:port` to try; repeatable.
        #[arg(long = "connect", required = true, num_args = 1..)]
        targets: Vec<String>,
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
        Command::Stage { work, tools } => agent::transfer::stage(&work, &tools),
        Command::Receive { work } => agent::transfer::receive(&work),
        Command::Hold => agent::transfer::hold(),
        Command::Tail { work, from } => agent::transfer::tail(&work, from),
        Command::Manifest { work } => agent::transfer::manifest(&work),
        Command::Send { work, name } => agent::transfer::send(&work, &name),
        Command::SshLink { socket } => agent::transfer::ssh_link(&socket),
        Command::Probe { timeout, targets } => {
            agent::transfer::probe(&targets, Duration::from_secs(timeout))
        }
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
    fn probe_takes_several_targets() {
        let cli = Cli::try_parse_from([
            "aurcache-build-agent",
            "probe",
            "--timeout",
            "5",
            "--connect",
            "a:1",
            "--connect",
            "b:2",
        ])
        .unwrap();
        let Some(Command::Probe { timeout, targets }) = cli.command else {
            panic!("not probe");
        };
        assert_eq!(timeout, 5);
        assert_eq!(targets, ["a:1", "b:2"]);
    }

    #[test]
    fn interface_stands_alone() {
        assert!(Cli::try_parse_from(["aurcache-build-agent", "--interface"]).is_ok());
        assert!(Cli::try_parse_from(["aurcache-build-agent", "--interface", "hold"]).is_err());
    }
}
