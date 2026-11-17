//! The other end of each `exec` a worker makes into a build pod: handing the
//! job in, following the log, handing the packages back, probing what a
//! build can reach, and carrying the SSH link.
//!
//! The wire format of each is in `kubernetes-executor-plan.md`, section 4.3.
//! Every subcommand exits 0 when it did what it was asked and 1 otherwise,
//! saying why on stderr; stdout carries only the payload (`manifest`'s JSON,
//! `send`'s file, `tail`'s log bytes, `probe`'s answer), so a caller can pass
//! it on untouched. Nothing here holds a package or a log in memory: every
//! copy goes through one fixed buffer.

mod file;
mod hold;
mod manifest;
mod probe;
mod receive;
mod send;
mod ssh_link;
mod stage;
mod tail;

use aurcache_build_agent::layout::{BIN_NAME, Work};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

/// Copy this executable to `<tools>/aurcache-build-agent`, create
/// `<work>/in/`, and exit 0 once `<work>/in/ready` exists.
pub fn stage(work: &Path, tools: &Path) -> ExitCode {
    finish(stage::run(&Work::new(work), tools))
}

/// Read a tar of `spec.json` and `source.tar.gz` from stdin into
/// `<work>/in/`, up to its end-of-archive marker, then create `<work>/in/ready`.
pub fn receive(work: &Path) -> ExitCode {
    finish(receive::run(&Work::new(work), std::io::stdin().lock()))
}

/// Sleep until signalled; exit 0 on `SIGTERM`.
pub fn hold() -> ExitCode {
    finish(hold::run())
}

/// Write `<work>/log/build.log` from byte `from`, following it, until the
/// report exists and everything up to the log's end was written.
pub fn tail(work: &Path, from: u64) -> ExitCode {
    finish(tail::run(&Work::new(work), from))
}

/// Print one `Handback` as JSON.
pub fn manifest(work: &Path) -> ExitCode {
    finish(manifest::run(&Work::new(work)))
}

/// Write the bytes of `<work>/pkg/<name>` to stdout, and nothing else.
pub fn send(work: &Path, name: &str) -> ExitCode {
    finish(send::run(&Work::new(work), name))
}

/// Try each `host:port` in parallel, for at most `timeout` each, and print
/// `[Reach]`. Exits 0 whatever it found.
pub fn probe(targets: &[String], timeout: Duration) -> ExitCode {
    finish(probe::run(targets, timeout))
}

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
