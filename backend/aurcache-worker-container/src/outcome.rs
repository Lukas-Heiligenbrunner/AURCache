//! How a build container ended, as one terminal report.
//!
//! In this order: the worker's own stop wins; then the agent's report; then,
//! when there is none, an interruption that is not the package's.

use aurcache_build_agent::report::{BuildReport, Ended, SetupStep};
use aurcache_common::worker::{BuildOutcome, CompleteReport};
use aurcache_worker_core::protocol::Stop;
use aurcache_worker_core::report::{Exit, ended, interrupted, success};
use std::time::Duration;

/// How a build container ended when the worker did not stop it.
#[derive(Clone, Debug, PartialEq)]
pub enum Ending {
    Reported(BuildReport),
    /// It ended with no report, for a reason that is not the package's.
    Unreported {
        detail: String,
    },
}

/// The terminal report for a build that ran for `elapsed`.
#[must_use]
pub fn complete(stop: Option<Stop>, elapsed: Duration, ending: Ending) -> CompleteReport {
    match ending {
        Ending::Reported(report) => from_report(stop, elapsed, &report),
        Ending::Unreported { detail } => match stop {
            Some(_) => ended(stop, elapsed, Exit::Unknown),
            None => interrupted(format!(
                "the build container ended without a report: {detail}"
            )),
        },
    }
}

/// The report for how the agent says the build ended.
fn from_report(stop: Option<Stop>, elapsed: Duration, report: &BuildReport) -> CompleteReport {
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
        Ended::Exited { .. } | Ended::Signaled { .. } => ended(None, elapsed, exit),
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

/// A step as the report spells it.
fn step_name(step: SetupStep) -> String {
    serde_json::to_value(step)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{step:?}").to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_worker_core::protocol::{Timeout, TimeoutSource};

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn reported(ended: Ended) -> Ending {
        Ending::Reported(BuildReport {
            interface: 1,
            ended,
        })
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
        let done = complete(None, secs(5), exited(0));
        assert_eq!(done.outcome, BuildOutcome::Succeeded);
        assert_eq!(done.exit_code, Some(0));
    }

    /// A package whose own build exits 124 failed; only the worker's stop is
    /// a timeout.
    #[test]
    fn a_failing_exit_or_a_signal_fails_the_build() {
        let done = complete(None, secs(5), exited(4));
        assert_eq!(done.outcome, BuildOutcome::Failed);
        assert_eq!(done.exit_code, Some(4));
        assert_eq!(done.reason.as_deref(), Some("build failed (exit 4)"));
        assert_eq!(
            complete(None, secs(5), exited(124)).outcome,
            BuildOutcome::Failed
        );
        let signaled = complete(None, secs(5), reported(Ended::Signaled { signal: 11 }));
        assert_eq!(signaled.outcome, BuildOutcome::Failed);
        assert_eq!(signaled.exit_code, None);
    }

    #[test]
    fn a_setup_failure_is_an_interruption_naming_the_step() {
        let done = complete(None, secs(5), setup(SetupStep::Keyring, "no route to host"));
        assert_eq!(done.outcome, BuildOutcome::Interrupted);
        let reason = done.reason.unwrap();
        assert!(reason.contains("keyring"), "{reason}");
        assert!(reason.contains("no route to host"), "{reason}");
    }

    #[test]
    fn a_dependency_that_cannot_be_installed_fails_the_build() {
        let message = "pacman -S --asdeps exited with status 1";
        let done = complete(None, secs(5), setup(SetupStep::Dependencies, message));
        assert_eq!(done.outcome, BuildOutcome::Failed);
        assert!(done.reason.unwrap().contains(message));
    }

    #[test]
    fn an_unreported_ending_is_an_interruption() {
        let ending = Ending::Unreported {
            detail: "the node was drained".into(),
        };
        let done = complete(None, secs(5), ending);
        assert_eq!(done.outcome, BuildOutcome::Interrupted);
        assert!(done.reason.unwrap().contains("the node was drained"));
    }

    #[test]
    fn the_workers_stop_wins_over_whatever_the_container_did() {
        let gone = || Ending::Unreported {
            detail: "gone".into(),
        };
        for ending in [exited(143), exited(0), gone(), setup(SetupStep::Sync, "x")] {
            let done = complete(Some(Stop::Canceled), secs(5), ending);
            assert_eq!(done.outcome, BuildOutcome::Canceled);
            assert_eq!(done.reason.as_deref(), Some("build canceled"));
        }
        for ending in [reported(Ended::Signaled { signal: 15 }), gone()] {
            let done = complete(Some(timeout()), secs(61), ending);
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
        assert_eq!(step_name(SetupStep::Credential), "credential");
    }
}
