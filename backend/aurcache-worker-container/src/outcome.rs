//! How a build container ended, as one terminal report.
//!
//! In this order: the worker's own stop wins; then the agent's report; then
//! what the engine said when there is none. Measurements in a report are
//! carried onto whatever report results.

use aurcache_build_agent::report::{BuildReport, Ended, SetupStep};
use aurcache_common::api::builds::DiskUsage;
use aurcache_common::source::is_object_name;
use aurcache_common::worker::{CompleteReport, JobVcsSource};
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

/// A report, as far as a worker passes it on.
#[derive(Clone, Debug, PartialEq)]
pub struct Vetted {
    pub report: BuildReport,
    /// The sources whose reported commits were left out.
    pub refused: Vec<String>,
}

/// What of `report` a worker passes on, for a job with VCS `sources`.
///
/// The report is a file in the build's directory, which root in the build
/// container could write. So its commits are held to the job: one per
/// source the job named, each a git object name. Any other is left out, and
/// the server keeps the commit it recorded when it queued the build.
#[must_use]
pub fn vetted(mut report: BuildReport, sources: &[JobVcsSource]) -> Vetted {
    let reported = std::mem::take(&mut report.vcs_commits);
    let mut refused = Vec::new();
    for (source, commit) in reported {
        let asked = sources.iter().any(|s| s.source_url == source);
        if asked && is_object_name(&commit) {
            report.vcs_commits.insert(source, commit);
        } else {
            refused.push(source);
        }
    }
    Vetted { report, refused }
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
#[must_use]
pub fn oom_reason(kills: Option<u64>, memory: Option<MemoryLimit>) -> String {
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
    use aurcache_common::worker::BuildOutcome;
    use aurcache_worker_core::protocol::{Timeout, TimeoutSource};
    use std::collections::BTreeMap;

    const GIB: u64 = 1 << 30;
    const LIMIT: MemoryLimit = MemoryLimit {
        bytes: 4 * GIB,
        variable: "WORKER_BUILD_MEMORY_MAX",
    };

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn report(ended: Ended) -> BuildReport {
        BuildReport {
            interface: 1,
            ended,
            peak_memory_bytes: Some(3 * GIB),
            oom_kills: Some(0),
            disk_bytes: Some(2 * GIB),
            vcs_commits: BTreeMap::from([(
                "git+https://x/y.git".to_string(),
                "abc123".to_string(),
            )]),
        }
    }

    fn exited(code: i32) -> Ending {
        Ending::Reported(report(Ended::Exited { code }))
    }

    fn timeout() -> Stop {
        Stop::TimedOut(Timeout {
            limit: secs(60),
            set_by: TimeoutSource::Server,
        })
    }

    #[test]
    fn a_clean_exit_succeeds_and_carries_the_measurements() {
        let done = complete(None, secs(5), exited(0), Some(LIMIT));
        assert_eq!(done.outcome, BuildOutcome::Succeeded);
        assert_eq!(done.exit_code, Some(0));
        assert_eq!(done.peak_memory_bytes, Some(3 * (1 << 30)));
        assert_eq!(done.vcs_commits["git+https://x/y.git"], "abc123");
        let disk = done.disk_usage.unwrap();
        assert_eq!(disk.workdir, Some(2 * (1 << 30)));
        assert_eq!(disk.chroot, None);
        assert_eq!(disk.sources, None);
        assert_eq!(disk.build_tree, None);
    }

    #[test]
    fn what_was_not_measured_stays_unknown() {
        let mut bare = report(Ended::Exited { code: 0 });
        bare.peak_memory_bytes = None;
        bare.disk_bytes = None;
        bare.vcs_commits.clear();
        let done = complete(None, secs(5), Ending::Reported(bare), None);
        assert_eq!(done.peak_memory_bytes, None);
        assert!(done.disk_usage.is_none());
        assert!(done.vcs_commits.is_empty());
    }

    #[test]
    fn a_failing_exit_fails_with_its_code_and_the_measurements() {
        let done = complete(None, secs(5), exited(4), Some(LIMIT));
        assert_eq!(done.outcome, BuildOutcome::Failed);
        assert_eq!(done.exit_code, Some(4));
        assert_eq!(done.reason.as_deref(), Some("build failed (exit 4)"));
        assert!(done.peak_memory_bytes.is_some());
    }

    /// A package whose own build exits 124 failed; only the worker's stop is
    /// a timeout.
    #[test]
    fn an_exit_124_is_not_a_timeout() {
        let done = complete(None, secs(5), exited(124), None);
        assert_eq!(done.outcome, BuildOutcome::Failed);
    }

    #[test]
    fn a_signal_fails_the_build() {
        let ending = Ending::Reported(report(Ended::Signaled { signal: 11 }));
        let done = complete(None, secs(5), ending, None);
        assert_eq!(done.outcome, BuildOutcome::Failed);
        assert_eq!(done.exit_code, None);
    }

    #[test]
    fn a_failure_with_oom_kills_is_worded_as_out_of_memory() {
        let mut killed = report(Ended::Exited { code: 2 });
        killed.oom_kills = Some(1);
        let done = complete(None, secs(5), Ending::Reported(killed), Some(LIMIT));
        assert_eq!(done.outcome, BuildOutcome::Failed);
        assert_eq!(done.exit_code, Some(2));
        let reason = done.reason.unwrap();
        assert!(reason.starts_with("out of memory"), "{reason}");
        assert!(reason.contains("WORKER_BUILD_MEMORY_MAX"), "{reason}");
        assert!(reason.contains("4.0 GiB"), "{reason}");
    }

    #[test]
    fn a_signaled_build_with_oom_kills_is_worded_as_out_of_memory() {
        let mut killed = report(Ended::Signaled { signal: 9 });
        killed.oom_kills = Some(3);
        let done = complete(None, secs(5), Ending::Reported(killed), Some(LIMIT));
        assert_eq!(done.outcome, BuildOutcome::Failed);
        assert!(done.reason.unwrap().contains("3 processes"));
    }

    /// Kills that did not stop a successful build are not its story.
    #[test]
    fn a_success_with_oom_kills_is_still_a_success() {
        let mut killed = report(Ended::Exited { code: 0 });
        killed.oom_kills = Some(1);
        let done = complete(None, secs(5), Ending::Reported(killed), Some(LIMIT));
        assert_eq!(done.outcome, BuildOutcome::Succeeded);
        assert_eq!(done.reason, None);
    }

    #[test]
    fn a_setup_failure_is_an_interruption_naming_the_step() {
        let ending = Ending::Reported(report(Ended::Setup {
            step: SetupStep::Keyring,
            message: "no route to host".into(),
        }));
        let done = complete(None, secs(5), ending, None);
        assert_eq!(done.outcome, BuildOutcome::Interrupted);
        let reason = done.reason.unwrap();
        assert!(reason.contains("keyring"), "{reason}");
        assert!(reason.contains("no route to host"), "{reason}");
    }

    #[test]
    fn the_engines_oom_kill_fails_the_build() {
        let done = complete(None, secs(5), Ending::OomKilled, Some(LIMIT));
        assert_eq!(done.outcome, BuildOutcome::Failed);
        assert_eq!(done.exit_code, Some(137));
        let reason = done.reason.unwrap();
        assert!(reason.starts_with("out of memory"), "{reason}");
        assert!(reason.contains("WORKER_BUILD_MEMORY_MAX"), "{reason}");
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
    fn a_cancel_wins_over_whatever_the_container_did() {
        let mut killed = report(Ended::Exited { code: 143 });
        killed.oom_kills = Some(2);
        for ending in [
            Ending::Reported(killed),
            exited(0),
            Ending::OomKilled,
            Ending::Unreported {
                detail: "gone".into(),
            },
            Ending::Reported(report(Ended::Setup {
                step: SetupStep::Sync,
                message: "x".into(),
            })),
        ] {
            let done = complete(Some(Stop::Canceled), secs(5), ending, Some(LIMIT));
            assert_eq!(done.outcome, BuildOutcome::Canceled);
            assert_eq!(done.reason.as_deref(), Some("build canceled"));
        }
    }

    #[test]
    fn a_timeout_wins_and_says_whose_limit() {
        let mut killed = report(Ended::Signaled { signal: 15 });
        killed.oom_kills = Some(1);
        let ending = Ending::Reported(killed);
        let done = complete(Some(timeout()), secs(61), ending, Some(LIMIT));
        assert_eq!(done.outcome, BuildOutcome::Failed);
        assert_eq!(done.exit_code, Some(124));
        let reason = done.reason.unwrap();
        assert!(reason.contains("timed out after 61s"), "{reason}");
        assert!(reason.contains("the package's job timeout"), "{reason}");
    }

    /// Whatever the build had measured before it was stopped still counts.
    #[test]
    fn a_stopped_build_keeps_its_measurements() {
        let done = complete(Some(Stop::Canceled), secs(5), exited(1), None);
        assert_eq!(done.peak_memory_bytes, Some(3 * (1 << 30)));
        assert_eq!(done.vcs_commits.len(), 1);
    }

    /// A stop is the whole story even when the engine has nothing to add.
    #[test]
    fn a_stop_without_a_report_still_ends_the_build() {
        let done = complete(Some(Stop::Canceled), secs(5), Ending::OomKilled, None);
        assert_eq!(done.outcome, BuildOutcome::Canceled);
        let ending = Ending::Unreported {
            detail: "gone".into(),
        };
        let done = complete(Some(timeout()), secs(61), ending, None);
        assert!(done.reason.unwrap().contains("timed out"));
        assert_eq!(done.peak_memory_bytes, None);
    }

    #[test]
    fn oom_reason_names_the_limit_and_its_variable() {
        let one = oom_reason(Some(1), Some(LIMIT));
        assert_eq!(
            one,
            "out of memory: the kernel killed a process at the build's memory limit of \
             4.0 GiB (WORKER_BUILD_MEMORY_MAX)"
        );
        let several = oom_reason(Some(5), Some(LIMIT));
        assert!(several.contains("5 processes"), "{several}");
    }

    #[test]
    fn oom_reason_without_a_limit_does_not_invent_one() {
        let reason = oom_reason(Some(1), None);
        assert!(reason.starts_with("out of memory"), "{reason}");
        assert!(!reason.contains("WORKER_"), "{reason}");
        assert!(reason.contains("a process"), "{reason}");
    }

    #[test]
    fn oom_reason_with_no_count_names_the_build() {
        let reason = oom_reason(None, Some(LIMIT));
        assert!(reason.contains("killed the build at"), "{reason}");
        assert_eq!(oom_reason(Some(0), None), oom_reason(None, None));
    }

    #[test]
    fn steps_are_named_as_the_report_spells_them() {
        assert_eq!(step_name(SetupStep::Keys), "keys");
        assert_eq!(step_name(SetupStep::Credential), "credential");
    }

    fn source(url: &str) -> JobVcsSource {
        JobVcsSource {
            source_url: url.to_string(),
            dir: "repo".to_string(),
            git_ref: "HEAD".to_string(),
        }
    }

    /// Only the job's own sources, and only object names: a report the
    /// build forged names what it likes.
    #[test]
    fn only_the_jobs_sources_and_object_names_are_passed_on() {
        let commit = "9fceb02d0ae598e95dc970b74767f19372d61af8";
        let mut written = report(Ended::Exited { code: 0 });
        written.vcs_commits = BTreeMap::from([
            ("git+https://x/y.git".to_string(), commit.to_string()),
            ("git+https://x/z.git".to_string(), "main".to_string()),
            ("anything".to_string(), commit.to_string()),
        ]);
        let sources = [source("git+https://x/y.git"), source("git+https://x/z.git")];

        let vetted = vetted(written.clone(), &sources);

        assert_eq!(
            vetted.report.vcs_commits,
            BTreeMap::from([("git+https://x/y.git".to_string(), commit.to_string())])
        );
        assert_eq!(vetted.refused, ["anything", "git+https://x/z.git"]);
        assert_eq!(vetted.report.ended, written.ended);
        assert_eq!(vetted.report.peak_memory_bytes, written.peak_memory_bytes);
    }
}
