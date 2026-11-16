//! How a build container ended, as one terminal report.
//!
//! In this order: the worker's own stop wins; then the agent's report; then
//! what the engine said when there is none. Measurements in a report are
//! carried onto whatever report results. The mapping row by row is in
//! `kubernetes-executor-plan.md`, section 4.4.

use aurcache_build_agent::report::BuildReport;
use aurcache_common::worker::CompleteReport;
use aurcache_worker_core::protocol::Stop;
use std::time::Duration;

/// A worker's own memory limit, for naming in an out-of-memory failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryLimit {
    pub bytes: u64,
    /// The setting an operator changes to raise it.
    pub variable: &'static str,
}

/// How a build container ended when the worker did not stop it.
#[derive(Clone, Debug, PartialEq)]
pub enum Ending {
    Reported(BuildReport),
    /// The engine says the kernel's OOM killer ended the container.
    OomKilled,
    /// It ended with no report, for a reason that is not the package's.
    Unreported {
        detail: String,
    },
}

/// The terminal report for a build that ran for `elapsed`.
#[must_use]
pub fn complete(
    stop: Option<Stop>,
    elapsed: Duration,
    ending: Ending,
    memory: Option<MemoryLimit>,
) -> CompleteReport {
    // WP3 writes this.
    let _ = (stop, elapsed, ending, memory);
    unimplemented!()
}

/// Why a build failed for memory, naming the limit when there is one.
#[must_use]
pub fn oom_reason(kills: Option<u64>, memory: Option<MemoryLimit>) -> String {
    // WP3 writes this.
    let _ = (kills, memory);
    unimplemented!()
}
