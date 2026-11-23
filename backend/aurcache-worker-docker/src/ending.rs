//! How a build container ended, from what the agent left and the status the
//! engine reports.

use aurcache_worker_container::outcome::Ending;

use crate::workdir::Report;

/// The ending for a container that left `report` and exited with `status`.
///
/// The agent exits 0 if and only if it wrote its report. A report beside any
/// other status, or none, was left by something else (root in the
/// container, before the container was killed whole) and is not believed.
#[must_use]
pub fn ending(report: Report, status: Option<i64>) -> Ending {
    match report {
        Report::Written(report) if status == Some(0) => Ending::Reported(report),
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
    use aurcache_build_agent::spec::INTERFACE;

    fn written() -> Report {
        Report::Written(BuildReport {
            interface: INTERFACE,
            ended: Ended::Exited { code: 1 },
        })
    }

    #[test]
    fn a_report_beside_an_agent_that_exited_0_is_taken_as_written() {
        let Report::Written(report) = written() else {
            unreachable!()
        };
        assert_eq!(ending(written(), Some(0)), Ending::Reported(report));
    }

    /// The agent writes a report exactly when it exits 0.
    #[test]
    fn anything_else_is_unreported_and_says_why() {
        for (report, status, why) in [
            (written(), Some(143), "143"),
            (written(), None, "no exit status"),
            (Report::Absent, Some(137), "137"),
            (Report::Unreadable("bad json".into()), Some(0), "bad json"),
        ] {
            let Ending::Unreported { detail } = ending(report, status) else {
                panic!("expected an unreported ending for {status:?}");
            };
            assert!(detail.contains(why), "{detail}");
        }
    }
}
