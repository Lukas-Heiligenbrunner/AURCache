//! How a build container ended, from what the agent left and what the engine
//! says about it.

use aurcache_worker_container::outcome::Ending;

use crate::workdir::Report;

/// What the engine says about a container that has ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// The agent's exit status, when the engine reported one.
    pub status: Option<i64>,
    /// Whether the kernel's OOM killer ended a process in the container.
    pub oom_killed: bool,
}

/// The ending for a container that left `report`.
///
/// The agent exits 0 if and only if it wrote its report. A report beside any
/// other status, or none, was left by something else (root in the
/// container, before the container was killed whole) and is not believed.
/// The engine's out-of-memory verdict counts only then: a report counts the
/// kills itself.
#[must_use]
pub fn ending(report: Report, verdict: Verdict) -> Ending {
    let status = verdict.status;
    match report {
        Report::Written(report) if status == Some(0) => Ending::Reported(report),
        Report::Written(_) | Report::Absent if verdict.oom_killed => Ending::OomKilled,
        Report::Written(_) | Report::Absent => Ending::Unreported {
            detail: match status {
                Some(status) => format!("the build agent exited with status {status}"),
                None => "the engine gave no exit status".to_string(),
            },
        },
        Report::Unreadable(why) => Ending::Unreported {
            detail: format!("its report could not be read: {why}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_build_agent::report::{BuildReport, Ended};

    fn written() -> Report {
        Report::Written(BuildReport::unmeasured(Ended::Exited { code: 1 }))
    }

    const fn verdict(status: Option<i64>, oom_killed: bool) -> Verdict {
        Verdict { status, oom_killed }
    }

    /// Beside an agent that exited 0, the report wins, even over the
    /// engine's out-of-memory kill.
    #[test]
    fn a_report_beside_an_agent_that_exited_0_is_taken_as_written() {
        let Report::Written(report) = written() else {
            unreachable!()
        };
        for oom_killed in [false, true] {
            let ended = ending(written(), verdict(Some(0), oom_killed));
            assert_eq!(ended, Ending::Reported(report.clone()));
        }
    }

    /// The agent writes a report exactly when it exits 0. Without one, the
    /// engine's out-of-memory kill is the story; an unreadable report is no
    /// evidence of either.
    #[test]
    fn anything_else_is_unreported_or_out_of_memory() {
        assert_eq!(
            ending(written(), verdict(Some(137), true)),
            Ending::OomKilled
        );
        assert_eq!(
            ending(Report::Absent, verdict(Some(137), true)),
            Ending::OomKilled
        );
        for (report, verdict, why) in [
            (written(), verdict(Some(143), false), "143"),
            (written(), verdict(None, false), "no exit status"),
            (Report::Absent, verdict(Some(137), false), "137"),
            (
                Report::Unreadable("bad json".into()),
                verdict(Some(0), true),
                "bad json",
            ),
        ] {
            let Ending::Unreported { detail } = ending(report, verdict) else {
                panic!("expected an unreported ending for {verdict:?}");
            };
            assert!(detail.contains(why), "{detail}");
        }
    }
}
