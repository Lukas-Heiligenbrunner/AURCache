//! Terminal build reports, shared by every executor.
//!
//! A worker must always send exactly one [`CompleteReport`] per claimed job —
//! staying silent forces the server to wait out the lease and abandon it. These
//! constructors are the vocabulary for that report; how the build itself ran is
//! the executor's business.

use crate::protocol::PackagesRefused;
use aurcache_common::worker::{BuildOutcome, CompleteReport, JobDescriptor};
use std::process::ExitStatus;

/// Map a build process exit status into a terminal report.
#[must_use]
pub fn classify_exit(status: ExitStatus, canceled: bool) -> CompleteReport {
    if canceled {
        return canceled_report(status.code());
    }
    if status.success() {
        return success();
    }
    let code = status.code();
    let reason = match code {
        // Process-specific: a bare wait-status code has no timeout convention.
        Some(124) => "build timed out (exit 124)".to_string(),
        Some(c) => exit_code_reason(i64::from(c)),
        None => "build terminated by signal".to_string(),
    };
    failure(code, reason)
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

/// The failure reason for a bare non-zero exit code.
///
/// Shared by every executor that learns the outcome as a code rather than an
/// [`ExitStatus`]: the OOM wording and the generic shape are stated once, so a
/// change to either cannot update one executor and silently miss the other.
/// (The `None` arms stay per-executor — a signal-killed process and a
/// status-less container exit mean different things.)
#[must_use]
pub fn exit_code_reason(code: i64) -> String {
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

/// A terminal report for a build the worker killed after exceeding its timeout.
#[must_use]
pub fn timeout_failure(secs: u64) -> CompleteReport {
    failure(Some(124), format!("build timed out after {secs}s"))
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

    #[test]
    fn classifies_success() {
        let r = classify_exit(fake_status(0), false);
        assert_eq!(r.outcome, BuildOutcome::Succeeded);
        assert_eq!(r.exit_code, Some(0));
    }

    #[test]
    fn classifies_oom_as_terminal_failure() {
        let r = classify_exit(fake_status(137), false);
        assert_eq!(r.outcome, BuildOutcome::Failed);
        assert_eq!(r.exit_code, Some(137));
        assert!(r.reason.unwrap().contains("OOM"));
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

    #[test]
    fn classifies_cancel() {
        let r = classify_exit(fake_status(1), true);
        assert_eq!(r.outcome, BuildOutcome::Canceled);
    }

    #[cfg(unix)]
    fn fake_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
}
