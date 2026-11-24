//! The other end of each `exec` that the Kubernetes worker makes into a build
//! pod, which has no directory shared with the worker: handing the job in,
//! following the log, and handing the packages back.
//!
//! - `stage` installs this binary where the build container runs it, and
//!   keeps its container up until `receive` has taken the job and exited.
//! - `receive` reads the job from stdin ([`aurcache_build_agent::handin`]).
//! - `hold` keeps the pod up for the hand-back, until it is told to stop.
//! - `tail` writes the build log from an offset, following it until the
//!   report exists and the log was written to its end.
//! - `manifest` prints a `Handback`: the report, and the regular files in
//!   `pkg/` with their sizes. `send` writes one of them to stdout.
//!
//! Each exits 0 when it did what it was asked, and 1 otherwise, saying why on
//! stderr; stdout carries only the payload. What the build left is opened
//! with `aurcache_common::beneath`: root in the build container could have
//! put a link or a pipe at any name.

mod hold;
mod manifest;
mod receive;
mod send;
mod stage;
mod tail;

use aurcache_build_agent::layout::{BIN_NAME, Work};
use std::fs::File;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::path::Path;
use std::process::ExitCode;

pub fn stage(work: &Path, tools: &Path) -> ExitCode {
    finish(stage::run(&Work::new(work), tools))
}

pub fn receive(work: &Path) -> ExitCode {
    finish(receive::run(&Work::new(work), &mut io::stdin().lock()))
}

pub fn hold() -> ExitCode {
    finish(hold::run())
}

pub fn tail(work: &Path, from: u64) -> ExitCode {
    finish(tail::run(&Work::new(work), from))
}

pub fn manifest(work: &Path) -> ExitCode {
    finish(manifest::run(&Work::new(work)))
}

pub fn send(work: &Path, name: &str) -> ExitCode {
    finish(send::run(&Work::new(work), name))
}

fn finish(result: io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{BIN_NAME}: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The process's stdout as a file, unbuffered, for bytes that are not text.
/// It does not close the descriptor when dropped.
fn stdout() -> ManuallyDrop<File> {
    // SAFETY: descriptor 1 is open for the life of the process, and the
    // `ManuallyDrop` keeps this handle from closing it.
    ManuallyDrop::new(unsafe { File::from_raw_fd(libc::STDOUT_FILENO) })
}

/// An error that says what was being done.
fn context(what: &str) -> impl Fn(io::Error) -> io::Error + '_ {
    move |error| io::Error::new(error.kind(), format!("{what}: {error}"))
}
