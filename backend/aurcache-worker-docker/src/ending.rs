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
/// The agent's report first, even when the engine also saw an out-of-memory
/// kill: the agent counted those itself. The engine's out-of-memory verdict
/// only for a container that left none; anything else is not the package's.
#[must_use]
pub fn ending(report: Report, verdict: Verdict) -> Ending {
    match report {
        Report::Written(report) => Ending::Reported(report),
        Report::Absent if verdict.oom_killed => Ending::OomKilled,
        Report::Absent => Ending::Unreported {
            detail: match verdict.status {
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
    use aurcache_build_agent::spec::INTERFACE;
    use std::collections::BTreeMap;

    fn written() -> BuildReport {
        BuildReport {
            interface: INTERFACE,
            ended: Ended::Exited { code: 1 },
            peak_memory_bytes: Some(10),
            oom_kills: Some(1),
            disk_bytes: Some(20),
            vcs_commits: BTreeMap::new(),
        }
    }

    const fn verdict(status: Option<i64>, oom_killed: bool) -> Verdict {
        Verdict { status, oom_killed }
    }

    #[test]
    fn a_report_is_taken_as_written() {
        let ended = ending(Report::Written(written()), verdict(Some(0), false));
        assert_eq!(ended, Ending::Reported(written()));
    }

    #[test]
    fn a_report_wins_over_the_engines_oom_kill() {
        let ended = ending(Report::Written(written()), verdict(Some(0), true));
        assert_eq!(ended, Ending::Reported(written()));
    }

    #[test]
    fn the_engines_oom_kill_counts_only_without_a_report() {
        assert_eq!(
            ending(Report::Absent, verdict(Some(137), true)),
            Ending::OomKilled
        );
    }

    #[test]
    fn no_report_and_no_oom_kill_is_unreported_with_the_status() {
        let Ending::Unreported { detail } = ending(Report::Absent, verdict(Some(137), false))
        else {
            panic!("expected an unreported ending");
        };
        assert!(detail.contains("137"), "{detail}");

        let Ending::Unreported { detail } = ending(Report::Absent, verdict(None, false)) else {
            panic!("expected an unreported ending");
        };
        assert!(detail.contains("no exit status"), "{detail}");
    }

    /// A file that is not a report is no evidence about memory either.
    #[test]
    fn an_unreadable_report_is_unreported_even_after_an_oom_kill() {
        let ended = ending(
            Report::Unreadable("bad json".into()),
            verdict(Some(0), true),
        );
        let Ending::Unreported { detail } = ended else {
            panic!("expected an unreported ending");
        };
        assert!(detail.contains("bad json"), "{detail}");
    }
}
