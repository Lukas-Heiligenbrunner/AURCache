//! The other end of each `exec` a worker makes into a build pod: handing the
//! job in, following the log, handing the packages back, and probing what a
//! build can reach.
//!
//! The wire format of each is in `kubernetes-executor-plan.md`, section 4.3.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

/// Copy this executable to `<tools>/aurcache-build-agent`, create
/// `<work>/in/`, and exit 0 once `<work>/in/ready` exists.
pub fn stage(work: &Path, tools: &Path) -> ExitCode {
    // WP2 writes this.
    let _ = (work, tools);
    unimplemented!()
}

/// Read a tar of `spec.json` and `source.tar.gz` from stdin into
/// `<work>/in/`, up to its end-of-archive marker, then create `<work>/in/ready`.
pub fn receive(work: &Path) -> ExitCode {
    // WP2 writes this.
    let _ = work;
    unimplemented!()
}

/// Sleep until signalled; exit 0 on `SIGTERM`.
pub fn hold() -> ExitCode {
    // WP2 writes this.
    unimplemented!()
}

/// Write `<work>/log/build.log` from byte `from`, following it, until the
/// report exists and everything up to the log's end was written.
pub fn tail(work: &Path, from: u64) -> ExitCode {
    // WP2 writes this.
    let _ = (work, from);
    unimplemented!()
}

/// Print one `Handback` as JSON.
pub fn manifest(work: &Path) -> ExitCode {
    // WP2 writes this.
    let _ = work;
    unimplemented!()
}

/// Write the bytes of `<work>/pkg/<name>` to stdout, and nothing else.
pub fn send(work: &Path, name: &str) -> ExitCode {
    // WP2 writes this.
    let _ = (work, name);
    unimplemented!()
}

/// Try each `host:port` in parallel, for at most `timeout` each, and print
/// `[Reach]`. Exits 0 whatever it found.
pub fn probe(targets: &[String], timeout: Duration) -> ExitCode {
    // WP2 writes this.
    let _ = (targets, timeout);
    unimplemented!()
}
