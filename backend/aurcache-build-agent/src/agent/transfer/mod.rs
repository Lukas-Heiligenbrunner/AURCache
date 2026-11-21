//! The other end of each `exec` a worker makes into a build pod: handing the
//! job in, following the log, handing the packages back, and carrying the
//! SSH link.
//!
//! Every subcommand exits 0 when it did what it was asked and 1 otherwise,
//! saying why on stderr; stdout carries only the payload (`manifest`'s JSON,
//! `send`'s file, `tail`'s log bytes), so a caller can pass
//! it on untouched. Nothing here holds a package or a log in memory: every
//! copy goes through one fixed buffer.
//!
//! What each reads and writes, exactly:
//!
//! - `stage --work <dir> --tools <dir>` copies its own executable to
//!   `<tools>/aurcache-build-agent` (mode 0755), creates `<work>/in/`, and
//!   exits 0 once `<work>/in/ready` exists and the process it names has
//!   exited (at most 30 seconds later).
//! - `receive --work <dir>` reads an uncompressed tar from stdin holding the
//!   regular files `spec.json` and `source.tar.gz`, in either order, each
//!   once, and nothing else; any other entry is refused. It stops reading at
//!   the end-of-archive marker, without waiting for the end of its input,
//!   writes both under `<work>/in/`, then creates `<work>/in/ready`, which
//!   holds its process id. A job already handed in is refused.
//! - `hold` sleeps until `SIGTERM` or `SIGINT`, and exits 0.
//! - `tail --work <dir> --from <offset>` writes `<work>/log/build.log` from
//!   byte `<offset>` to stdout and keeps following it, waiting for it to
//!   appear. It exits 0 once `<work>/out/report.json` exists and everything
//!   up to the end of the log was written.
//! - `manifest --work <dir>` prints one `Handback` as a line of JSON: the
//!   build's report, `null` when there is none, and as `artifacts` the
//!   entries of `<work>/pkg/` that are regular files and pass
//!   `is_artifact`, sorted by name, with their sizes.
//! - `send --work <dir> --name <file>` refuses a name that fails
//!   `is_artifact`, opens `<work>/pkg/<name>` without following a link,
//!   requires a regular file, and writes its bytes to stdout and nothing
//!   else.
//! - `ssh-link --socket <path>` connects to the agent's link socket and
//!   copies bytes between it and stdin and stdout until either side ends.
//!
//! `pkg/`, `out/` and `log/` are opened as directories of the work
//! directory, never through a link at their place: root in the build
//! container could have put anything there.

mod file;
mod hold;
mod manifest;
mod receive;
mod send;
mod ssh_link;
mod stage;
mod tail;

use aurcache_build_agent::layout::{BIN_NAME, Work};
use std::path::Path;
use std::process::ExitCode;

/// Copy this executable to `<tools>/aurcache-build-agent`, create
/// `<work>/in/`, and exit 0 once `<work>/in/ready` exists and the `receive`
/// it names has exited.
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
