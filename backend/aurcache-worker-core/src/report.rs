//! Terminal build reports, shared by every executor.
//!
//! A worker must always send exactly one [`CompleteReport`] per claimed job —
//! staying silent forces the server to wait out the lease and abandon it. These
//! constructors are the vocabulary for that report; how the build itself ran is
//! the executor's business.

use crate::protocol::{PackagesRefused, Stop, Timeout, TimeoutSource};
use aurcache_common::worker::{BuildOutcome, CompleteReport, JobDescriptor};
use std::process::ExitStatus;
use std::time::Duration;

/// How a build's process ended, as far as its executor could tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// It exited with this code.
    Code(i64),
    /// A signal ended it, so there is no code.
    Signal,
    /// The executor learned nothing: a container that vanished without a
    /// status.
    Unknown,
}

impl From<ExitStatus> for Exit {
    fn from(status: ExitStatus) -> Self {
        status
            .code()
            .map_or(Self::Signal, |code| Self::Code(i64::from(code)))
    }
}

/// The terminal report for a build whose process ended: `stop` when the
/// worker stopped it, after `elapsed`, and otherwise what `exit` says.
///
/// One mapping for every executor, so a build ends for the same reason
/// whichever one ran it. A code is only ever the package's: a timeout is the
/// worker's own [`Stop`], never inferred from an exit code, so a build that
/// itself exits 124 is a failure like any other.
#[must_use]
pub fn ended(stop: Option<Stop>, elapsed: Duration, exit: Exit) -> CompleteReport {
    let code = match exit {
        Exit::Code(code) => Some(code),
        Exit::Signal | Exit::Unknown => None,
    };
    match stop {
        Some(Stop::TimedOut(limit)) => timed_out(elapsed, limit),
        Some(Stop::Canceled) => canceled_report(code.and_then(|c| i32::try_from(c).ok())),
        None => match exit {
            Exit::Code(0) => success(),
            Exit::Code(code) => failure(i32::try_from(code).ok(), exit_code_reason(code)),
            Exit::Signal => failure(None, "build terminated by signal".to_string()),
            Exit::Unknown => interrupted("build exited without a status"),
        },
    }
}

/// A terminal report for a build that succeeded, with nothing measured.
#[must_use]
pub fn success() -> CompleteReport {
    CompleteReport {
        outcome: BuildOutcome::Succeeded,
        exit_code: Some(0),
        ..CompleteReport::default()
    }
}

/// The failure reason for a non-zero exit code.
fn exit_code_reason(code: i64) -> String {
    match code {
        137 => "build killed (OOM, exit 137)".to_string(),
        c => format!("build failed (exit {c})"),
    }
}

/// A terminal report for a build this worker could not see through, for
/// reasons of its own rather than the package's: see
/// [`BuildOutcome::Interrupted`].
#[must_use]
pub fn interrupted(reason: impl std::fmt::Display) -> CompleteReport {
    CompleteReport {
        outcome: BuildOutcome::Interrupted,
        ..failure(None, reason.to_string())
    }
}

/// A terminal report for a build aborted before it started (cancel observed
/// during setup, so there is no process exit status).
#[must_use]
pub fn canceled() -> CompleteReport {
    canceled_report(None)
}

/// The shared "build canceled" shape, with whatever exit code is available.
fn canceled_report(exit_code: Option<i32>) -> CompleteReport {
    CompleteReport {
        outcome: BuildOutcome::Canceled,
        ..failure(exit_code, "build canceled".to_string())
    }
}

/// A terminal report for a build the worker killed after `elapsed`, for
/// running past `limit`. Says whose limit it was: raising the wrong one
/// changes nothing.
fn timed_out(elapsed: Duration, limit: Timeout) -> CompleteReport {
    let whose = match limit.set_by {
        TimeoutSource::Worker => "this worker's build timeout",
        TimeoutSource::Server => "the package's job timeout",
    };
    // A whole number of minutes or more reads with its unit; anything else
    // is a bare count of seconds, which needs one.
    let mut limit = aurcache_common::units::format_duration(limit.limit.as_secs());
    if limit.bytes().all(|b| b.is_ascii_digit()) {
        limit.push('s');
    }
    failure(
        Some(124),
        format!(
            "build timed out after {}s ({whose}, {limit})",
            elapsed.as_secs()
        ),
    )
}

/// A failed build with nothing measured about it.
fn failure(exit_code: Option<i32>, reason: String) -> CompleteReport {
    CompleteReport {
        outcome: BuildOutcome::Failed,
        exit_code,
        reason: Some(reason),
        ..CompleteReport::default()
    }
}

/// The terminal report for a job that ended in an error rather than a build
/// result.
///
/// Failed when what went wrong was the build's packages being refused --
/// none made, one not a regular file, or one the server would not take: the
/// package's doing, which a retry repeats. Interrupted otherwise: preparing
/// the build, fetching its sources and delivering its packages are this
/// worker's to get right.
#[must_use]
pub fn from_error(error: &anyhow::Error) -> CompleteReport {
    let reason = format!("{error:#}");
    if error.downcast_ref::<PackagesRefused>().is_some() {
        failure(None, reason)
    } else {
        interrupted(reason)
    }
}

/// Short human label for a job, used in worker logs.
#[must_use]
pub fn describe(job: &JobDescriptor) -> String {
    format!("{} ({})", job.pkgbase, job.arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn secs(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    fn timed_out_reason(elapsed: u64, limit: u64, set_by: TimeoutSource) -> String {
        let limit = Timeout {
            limit: secs(limit),
            set_by,
        };
        ended(Some(Stop::TimedOut(limit)), secs(elapsed), Exit::Signal)
            .reason
            .unwrap()
    }

    /// Raising the wrong limit changes nothing, so the report says which.
    #[test]
    fn a_timeout_names_the_limit_that_ended_it() {
        assert_eq!(
            timed_out_reason(3601, 3600, TimeoutSource::Server),
            "build timed out after 3601s (the package's job timeout, 1h)"
        );
        let reason = timed_out_reason(10801, 3 * 3600, TimeoutSource::Worker);
        assert!(
            reason.contains("this worker's build timeout, 3h"),
            "{reason}"
        );
        let reason = timed_out_reason(90, 90, TimeoutSource::Server);
        assert!(reason.ends_with("job timeout, 90s)"), "{reason}");
    }

    #[test]
    fn classifies_success() {
        let r = ended(None, secs(1), fake_status(0).into());
        assert_eq!(r.outcome, BuildOutcome::Succeeded);
        assert_eq!(r.exit_code, Some(0));
    }

    #[test]
    fn classifies_oom_as_terminal_failure() {
        let r = ended(None, secs(1), fake_status(137).into());
        assert_eq!(r.outcome, BuildOutcome::Failed);
        assert_eq!(r.exit_code, Some(137));
        assert!(r.reason.unwrap().contains("OOM"));
    }

    /// Only the worker's own stop is a timeout: a package whose build exits
    /// 124 (a `timeout` in its test suite, say) failed.
    #[test]
    fn exit_124_is_a_failure_like_any_other() {
        let r = ended(None, secs(1), Exit::Code(124));
        assert_eq!(r.outcome, BuildOutcome::Failed);
        assert_eq!(r.reason.as_deref(), Some("build failed (exit 124)"));
    }

    /// A container that vanished without a status is the worker's to
    /// explain, not the package's.
    #[test]
    fn no_status_is_an_interruption() {
        let r = ended(None, secs(1), Exit::Unknown);
        assert_eq!(r.outcome, BuildOutcome::Interrupted);
    }

    #[test]
    fn refused_packages_fail_the_build() {
        let refused = anyhow::Error::new(PackagesRefused("no packages".into()))
            .context("uploading hello-1-1-x86_64.pkg.tar.zst");
        assert_eq!(from_error(&refused).outcome, BuildOutcome::Failed);

        let unreachable = anyhow::anyhow!("connection reset").context("downloading source");
        let report = from_error(&unreachable);
        assert_eq!(report.outcome, BuildOutcome::Interrupted);
        assert_eq!(
            report.reason.as_deref(),
            Some("downloading source: connection reset")
        );
    }

    /// Whatever the process did on its way out, a stop the worker made is
    /// what is reported.
    #[test]
    fn classifies_cancel() {
        let r = ended(Some(Stop::Canceled), secs(1), fake_status(1).into());
        assert_eq!(r.outcome, BuildOutcome::Canceled);
        assert_eq!(r.exit_code, Some(1));
        let r = ended(Some(Stop::Canceled), secs(1), Exit::Signal);
        assert_eq!(r.outcome, BuildOutcome::Canceled);
    }

    #[cfg(unix)]
    fn fake_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
}
