//! How a build container ended, as one terminal report.
//!
//! In this order: the worker's own stop wins; then the agent's report; then
//! what the engine said when there is none. Measurements in a report are
//! carried onto whatever report results.

use aurcache_build_agent::report::{BuildReport, Ended, SetupStep};
use aurcache_common::api::builds::DiskUsage;
use aurcache_common::worker::{BuildOutcome, CompleteReport};
use aurcache_worker_core::protocol::Stop;
use aurcache_worker_core::report::{Exit, ended, interrupted, success};
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

/// The exit code of a process the kernel ended with `SIGKILL`.
const KILLED: i64 = 137;

/// The terminal report for a build that ran for `elapsed`.
#[must_use]
pub fn complete(
    stop: Option<Stop>,
    elapsed: Duration,
    ending: Ending,
    memory: Option<MemoryLimit>,
) -> CompleteReport {
    match ending {
        Ending::Reported(report) => {
            let result = from_report(stop, elapsed, &report, memory);
            measured(result, &report)
        }
        Ending::OomKilled => {
            let mut result = ended(stop, elapsed, Exit::Code(KILLED));
            if stop.is_none() {
                result.reason = Some(oom_reason(None, memory));
            }
            result
        }
        Ending::Unreported { detail } => match stop {
            Some(_) => ended(stop, elapsed, Exit::Unknown),
            None => interrupted(format!(
                "the build container ended without a report: {detail}"
            )),
        },
    }
}

/// The report for how the agent says the build ended.
fn from_report(
    stop: Option<Stop>,
    elapsed: Duration,
    report: &BuildReport,
    memory: Option<MemoryLimit>,
) -> CompleteReport {
    let exit = match &report.ended {
        Ended::Exited { code } => Exit::Code(i64::from(*code)),
        Ended::Signaled { .. } => Exit::Signal,
        Ended::Setup { .. } => Exit::Unknown,
    };
    if stop.is_some() {
        return ended(stop, elapsed, exit);
    }
    match &report.ended {
        Ended::Exited { code: 0 } => success(),
        Ended::Exited { .. } | Ended::Signaled { .. } => {
            let mut result = ended(None, elapsed, exit);
            if report.oom_kills.is_some_and(|kills| kills > 0) {
                result.reason = Some(oom_reason(report.oom_kills, memory));
            }
            result
        }
        // A dependency that cannot be installed is the package's, as it is
        // when makepkg installs it, and a retry finds it missing again.
        Ended::Setup {
            step: SetupStep::Dependencies,
            message,
        } => CompleteReport {
            outcome: BuildOutcome::Failed,
            reason: Some(format!(
                "the build's dependencies could not be installed: {message}"
            )),
            ..CompleteReport::default()
        },
        Ended::Setup { step, message } => interrupted(format!(
            "the build could not start: {} failed: {message}",
            step_name(*step)
        )),
    }
}

/// `result` with what the agent measured copied onto it.
fn measured(mut result: CompleteReport, report: &BuildReport) -> CompleteReport {
    result.peak_memory_bytes = report
        .peak_memory_bytes
        .and_then(|bytes| i64::try_from(bytes).ok());
    result.vcs_commits.clone_from(&report.vcs_commits);
    result.disk_usage = report
        .disk_bytes
        .and_then(|bytes| i64::try_from(bytes).ok())
        .map(|bytes| DiskUsage {
            workdir: Some(bytes),
            ..DiskUsage::default()
        });
    result
}

/// A step as the report spells it.
fn step_name(step: SetupStep) -> String {
    serde_json::to_value(step)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{step:?}").to_lowercase())
}

/// Why a build failed for memory, naming the limit when there is one.
fn oom_reason(kills: Option<u64>, memory: Option<MemoryLimit>) -> String {
    let killed = match kills {
        Some(1) => "a process".to_string(),
        Some(many) if many > 1 => format!("{many} processes"),
        _ => "the build".to_string(),
    };
    match memory {
        Some(limit) => format!(
            "out of memory: the kernel killed {killed} at the build's memory limit of \
             {:.1} GiB ({})",
            limit.bytes as f64 / f64::from(1u32 << 30),
            limit.variable
        ),
        None => format!(
            "out of memory: the kernel killed {killed} in the build, at a limit the worker \
             did not set (the machine, or what runs the worker)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_worker_core::protocol::{Timeout, TimeoutSource};
    use std::collections::BTreeMap;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn reported(ended: Ended) -> Ending {
        Ending::Reported(BuildReport::unmeasured(ended))
    }

    fn exited(code: i32) -> Ending {
        reported(Ended::Exited { code })
    }

    fn setup(step: SetupStep, message: &str) -> Ending {
        reported(Ended::Setup {
            step,
            message: message.into(),
        })
    }

    fn timeout() -> Stop {
        Stop::TimedOut(Timeout {
            limit: secs(60),
            set_by: TimeoutSource::Server,
        })
    }

    #[test]
    fn a_clean_exit_succeeds() {
        let done = complete(None, secs(5), exited(0), None);
        assert_eq!(done.outcome, BuildOutcome::Succeeded);
        assert_eq!(done.exit_code, Some(0));
    }

    /// A package whose own build exits 124 failed; only the worker's stop is
    /// a timeout.
    #[test]
    fn a_failing_exit_or_a_signal_fails_the_build() {
        let done = complete(None, secs(5), exited(4), None);
        assert_eq!(done.outcome, BuildOutcome::Failed);
        assert_eq!(done.exit_code, Some(4));
        assert_eq!(done.reason.as_deref(), Some("build failed (exit 4)"));
        assert_eq!(
            complete(None, secs(5), exited(124), None).outcome,
            BuildOutcome::Failed
        );
        let signaled = complete(
            None,
            secs(5),
            reported(Ended::Signaled { signal: 11 }),
            None,
        );
        assert_eq!(signaled.outcome, BuildOutcome::Failed);
        assert_eq!(signaled.exit_code, None);
    }

    #[test]
    fn a_setup_failure_is_an_interruption_naming_the_step() {
        let done = complete(
            None,
            secs(5),
            setup(SetupStep::Keyring, "no route to host"),
            None,
        );
        assert_eq!(done.outcome, BuildOutcome::Interrupted);
        let reason = done.reason.unwrap();
        assert!(reason.contains("keyring"), "{reason}");
        assert!(reason.contains("no route to host"), "{reason}");
    }

    #[test]
    fn a_dependency_that_cannot_be_installed_fails_the_build() {
        let message = "pacman -S --asdeps exited with status 1";
        let done = complete(None, secs(5), setup(SetupStep::Dependencies, message), None);
        assert_eq!(done.outcome, BuildOutcome::Failed);
        assert!(done.reason.unwrap().contains(message));
    }

    #[test]
    fn an_unreported_ending_is_an_interruption() {
        let ending = Ending::Unreported {
            detail: "the node was drained".into(),
        };
        let done = complete(None, secs(5), ending, None);
        assert_eq!(done.outcome, BuildOutcome::Interrupted);
        assert!(done.reason.unwrap().contains("the node was drained"));
    }

    #[test]
    fn the_workers_stop_wins_over_whatever_the_container_did() {
        let gone = || Ending::Unreported {
            detail: "gone".into(),
        };
        for ending in [exited(143), exited(0), gone(), setup(SetupStep::Sync, "x")] {
            let done = complete(Some(Stop::Canceled), secs(5), ending, None);
            assert_eq!(done.outcome, BuildOutcome::Canceled);
            assert_eq!(done.reason.as_deref(), Some("build canceled"));
        }
        for ending in [reported(Ended::Signaled { signal: 15 }), gone()] {
            let done = complete(Some(timeout()), secs(61), ending, None);
            assert_eq!(done.outcome, BuildOutcome::Failed);
            assert_eq!(done.exit_code, Some(124));
            let reason = done.reason.unwrap();
            assert!(reason.contains("timed out after 61s"), "{reason}");
            assert!(reason.contains("the package's job timeout"), "{reason}");
        }
    }

    #[test]
    fn steps_are_named_as_the_report_spells_them() {
        assert_eq!(step_name(SetupStep::Keys), "keys");
    }

    const GIB: u64 = 1 << 30;
    const LIMIT: MemoryLimit = MemoryLimit {
        bytes: 4 * GIB,
        variable: "WORKER_BUILD_MEMORY_MAX",
    };

    fn measured_report(ended: Ended, oom_kills: u64) -> BuildReport {
        BuildReport {
            peak_memory_bytes: Some(3 * GIB),
            oom_kills: Some(oom_kills),
            disk_bytes: Some(2 * GIB),
            vcs_commits: BTreeMap::from([("git+https://x/y.git".into(), "abc123".into())]),
            ..BuildReport::unmeasured(ended)
        }
    }

    /// The measurements are carried onto the report, stopped or not, and
    /// what was not measured stays unknown.
    #[test]
    fn measurements_are_carried_on() {
        for stop in [None, Some(Stop::Canceled)] {
            let ending = Ending::Reported(measured_report(Ended::Exited { code: 0 }, 0));
            let done = complete(stop, secs(5), ending, Some(LIMIT));
            assert_eq!(done.peak_memory_bytes, Some(3 * (1 << 30)));
            assert_eq!(done.vcs_commits["git+https://x/y.git"], "abc123");
            let disk = done.disk_usage.unwrap();
            assert_eq!(disk.workdir, Some(2 * (1 << 30)));
            assert_eq!(disk.chroot, None);
        }
        let done = complete(None, secs(5), exited(0), None);
        assert_eq!(done.peak_memory_bytes, None);
        assert!(done.disk_usage.is_none());
    }

    /// A failure with out-of-memory kills, or the engine's word that the
    /// container was killed for memory, is worded so and names the limit; a
    /// success with kills is still a success, and a stop still wins.
    #[test]
    fn out_of_memory_is_worded_so() {
        for (ending, killed) in [
            (
                Ending::Reported(measured_report(Ended::Exited { code: 2 }, 1)),
                "a process",
            ),
            (
                Ending::Reported(measured_report(Ended::Signaled { signal: 9 }, 3)),
                "3 processes",
            ),
            (Ending::OomKilled, "the build"),
        ] {
            let done = complete(None, secs(5), ending.clone(), Some(LIMIT));
            assert_eq!(done.outcome, BuildOutcome::Failed);
            let reason = done.reason.unwrap();
            assert!(reason.contains(killed), "{reason}");
            assert!(
                reason.contains("4.0 GiB (WORKER_BUILD_MEMORY_MAX)"),
                "{reason}"
            );
            let stopped = complete(Some(Stop::Canceled), secs(5), ending, Some(LIMIT));
            assert_eq!(stopped.outcome, BuildOutcome::Canceled);
        }
        let ending = Ending::Reported(measured_report(Ended::Exited { code: 0 }, 1));
        assert_eq!(complete(None, secs(5), ending, Some(LIMIT)).reason, None);
        let unlimited = oom_reason(Some(1), None);
        assert!(!unlimited.contains("WORKER_"), "{unlimited}");
    }
}
