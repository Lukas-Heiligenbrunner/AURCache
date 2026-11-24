//! How a build pod is doing and how it ended, from its status and the
//! hand-back, as pure functions.
//!
//! [`observe`] reads where a pod is from its status. A pod whose build
//! container ended is judged by [`ending`], from what the container's status
//! and the agent's report say; a pod that can no longer hand anything back is
//! reported by [`lost`]. The other ways a build ends without a pod to read
//! have a report each: [`refused`], [`unstarted`], [`gone`].

use crate::pod::{BUILD, HOLD, STAGE};
use aurcache_build_agent::report::BuildReport;
use aurcache_common::worker::CompleteReport;
use aurcache_worker_container::outcome::Ending;
use aurcache_worker_core::report::{Exit, ended, interrupted};
use k8s_openapi::api::core::v1::{ContainerState, ContainerStatus, Pod, PodStatus};
use std::time::Duration;

/// The variable that raises the per-build disk limit.
const DISK_VARIABLE: &str = "WORKER_BUILD_DISK_MAX";
/// The variable that says how long a pod may take to start its build.
const SCHEDULE_VARIABLE: &str = "WORKER_KUBE_SCHEDULE_TIMEOUT";

/// Where a build pod is, as far as its status says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observed {
    /// No node has taken it. `unschedulable` is the scheduler's message once
    /// it has looked and found none.
    Unscheduled { unschedulable: Option<String> },
    /// On a node, with `stage` not running yet. `waiting` is the kubelet's
    /// reason and message, such as an image it cannot pull.
    Starting {
        node: Option<String>,
        waiting: Option<String>,
    },
    /// `stage` runs: the pod is waiting for the job.
    Staged,
    /// The job is in, and the build container does not run yet. `waiting`
    /// is the kubelet's reason, such as an image it cannot pull.
    Building { waiting: Option<String> },
    /// The build container runs, and can be exec'd into.
    Running,
    /// The build container ended, and `hold` keeps the pod up for the
    /// hand-back.
    Built(BuildEnd),
    /// The pod can no longer produce or hand back a build.
    Lost(Loss),
}

/// How the build container ended, as its status has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildEnd {
    pub exit_code: i32,
    /// Whether the kubelet says the container was killed for its memory.
    pub oom_killed: bool,
    /// The status in a few words, for a report that has nothing better.
    pub detail: String,
}

/// Why a pod can no longer produce or hand back a build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loss {
    /// The kubelet evicted it for passing a storage limit of its own.
    OverLimit { message: String },
    /// The kubelet evicted it for the node's sake.
    Evicted { message: String },
    /// It ran past `activeDeadlineSeconds`.
    DeadlineExceeded,
    /// It ended some other way that leaves nothing to hand back.
    Failed { message: String },
}

/// Where `pod` is.
#[must_use]
pub fn observe(pod: &Pod) -> Observed {
    let Some(status) = &pod.status else {
        return Observed::Unscheduled {
            unschedulable: None,
        };
    };
    let stage = container(status.init_container_statuses.as_deref(), STAGE).map(state);
    if let Some(loss) = pod_loss(status, stage.as_ref()) {
        return Observed::Lost(loss);
    }
    if let Some(unscheduled) = unscheduled(status) {
        return unscheduled;
    }
    let node = pod.spec.as_ref().and_then(|spec| spec.node_name.clone());
    match stage {
        Some(State::Running) => return Observed::Staged,
        Some(State::Ended { .. }) => {}
        Some(State::Waiting { waiting }) => return Observed::Starting { node, waiting },
        None | Some(State::Unknown) => {
            return Observed::Starting {
                node,
                waiting: None,
            };
        }
    }
    building(status)
}

/// Where a pod whose `stage` container is done is.
fn building(status: &PodStatus) -> Observed {
    let build = container(status.container_statuses.as_deref(), BUILD).map(state);
    let hold = container(status.container_statuses.as_deref(), HOLD).map(state);
    if let Some(State::Ended {
        exit_code,
        oom_killed,
        detail,
    }) = &build
    {
        return Observed::Built(BuildEnd {
            exit_code: *exit_code,
            oom_killed: *oom_killed,
            detail: detail.clone(),
        });
    }
    if let Some(State::Ended { detail, .. }) = &hold {
        return Observed::Lost(Loss::Failed {
            message: format!("the hold container ended while the build ran: {detail}"),
        });
    }
    match build {
        Some(State::Running) => Observed::Running,
        Some(State::Waiting { waiting }) => Observed::Building { waiting },
        _ => Observed::Building { waiting: None },
    }
}

/// What says the pod is beyond recovery: its reason, a failed `stage`, then
/// its phase. Asked before any other container is looked at: an evicted pod's
/// containers are all gone, `hold` included.
fn pod_loss(status: &PodStatus, stage: Option<&State>) -> Option<Loss> {
    let message = status.message.clone().unwrap_or_default();
    match status.reason.as_deref() {
        Some("Evicted") if over_own_limit(&message) => return Some(Loss::OverLimit { message }),
        Some("Evicted") => return Some(Loss::Evicted { message }),
        Some("DeadlineExceeded") => return Some(Loss::DeadlineExceeded),
        _ => {}
    }
    if let Some(State::Ended {
        exit_code, detail, ..
    }) = stage
        && *exit_code != 0
    {
        return Some(Loss::Failed {
            message: format!("the stage container failed: {detail}"),
        });
    }
    // With `restartPolicy: Never` a pod reaches a final phase once every
    // container has ended, so nothing is left to exec into.
    let phase = status.phase.as_deref()?;
    if !matches!(phase, "Failed" | "Succeeded") {
        return None;
    }
    let reason = status.reason.as_deref().unwrap_or("its containers ended");
    Some(Loss::Failed {
        message: if message.is_empty() {
            format!("the pod is {phase}: {reason}")
        } else {
            format!("the pod is {phase}: {reason}: {message}")
        },
    })
}

/// Whether an eviction message says the pod passed a limit of its own. The
/// kubelet words those as a limit that was exceeded, and an eviction for the
/// node's sake as "The node was low on resource"; checked against kubelet
/// v1.37.0, `pkg/kubelet/eviction/helpers.go`.
fn over_own_limit(message: &str) -> bool {
    message.contains("exceed") && message.contains("limit")
}

/// [`Observed::Unscheduled`] while no node has taken the pod.
fn unscheduled(status: &PodStatus) -> Option<Observed> {
    let scheduled = status
        .conditions
        .iter()
        .flatten()
        .find(|condition| condition.type_ == "PodScheduled");
    let unschedulable = match scheduled {
        Some(condition) if condition.status == "True" => return None,
        Some(condition) if condition.reason.as_deref() == Some("Unschedulable") => Some(
            condition
                .message
                .clone()
                .unwrap_or_else(|| "no node can take it".to_string()),
        ),
        _ => None,
    };
    Some(Observed::Unscheduled { unschedulable })
}

/// The scheduler's message for a pod it found no node for; `None` for any
/// other pod.
#[must_use]
pub fn unschedulable(pod: &Pod) -> Option<String> {
    match observe(pod) {
        Observed::Unscheduled { unschedulable } => unschedulable,
        _ => None,
    }
}

/// One container's state, reduced to what the lifecycle reads.
enum State {
    /// The kubelet's reason, with its message when it has one.
    Waiting {
        waiting: Option<String>,
    },
    Running,
    Ended {
        exit_code: i32,
        oom_killed: bool,
        detail: String,
    },
    Unknown,
}

fn container<'a>(
    statuses: Option<&'a [ContainerStatus]>,
    name: &str,
) -> Option<&'a ContainerStatus> {
    statuses?.iter().find(|status| status.name == name)
}

fn state(status: &ContainerStatus) -> State {
    let Some(ContainerState {
        running,
        terminated,
        waiting,
    }) = &status.state
    else {
        return State::Unknown;
    };
    if let Some(ended) = terminated {
        let detail = match ended.reason.as_deref().filter(|reason| !reason.is_empty()) {
            Some(reason) => format!("exit code {}, {reason}", ended.exit_code),
            None => format!("exit code {}", ended.exit_code),
        };
        State::Ended {
            exit_code: ended.exit_code,
            oom_killed: ended.reason.as_deref() == Some("OOMKilled"),
            detail,
        }
    } else if running.is_some() {
        State::Running
    } else if let Some(waiting) = waiting {
        let message = waiting
            .message
            .as_deref()
            .filter(|message| !message.is_empty());
        State::Waiting {
            waiting: match (&waiting.reason, message) {
                (Some(reason), Some(message)) => Some(format!("{reason}: {message}")),
                (reason, _) => reason.clone(),
            },
        }
    } else {
        State::Unknown
    }
}

impl Observed {
    /// What the build log says while the pod is here; `None` where the
    /// lifecycle says something better itself.
    #[must_use]
    pub fn line(&self) -> Option<String> {
        let on = |node: &Option<String>| {
            node.as_ref()
                .map_or_else(String::new, |node| format!(" on node {node}"))
        };
        let why = |waiting: &Option<String>| {
            waiting
                .as_ref()
                .map_or_else(String::new, |reason| format!(" ({reason})"))
        };
        match self {
            Self::Unscheduled {
                unschedulable: None,
            } => Some("the build pod is waiting for a node".to_string()),
            Self::Unscheduled {
                unschedulable: Some(message),
            } => Some(format!("no node can take the build pod yet: {message}")),
            Self::Starting { node, waiting } => Some(format!(
                "the build pod is starting{}{}",
                on(node),
                why(waiting)
            )),
            Self::Building {
                waiting: Some(reason),
            } => Some(format!("the build container is starting ({reason})")),
            Self::Building { waiting: None } => Some("the build container is starting".to_string()),
            Self::Staged => Some("the build pod is ready for the job".to_string()),
            Self::Running => Some("the build is running".to_string()),
            Self::Built(_) | Self::Lost(_) => None,
        }
    }
}

/// How a build container that ended on its own is to be reported: by the
/// agent's report when it wrote one, and otherwise by the kubelet's
/// out-of-memory kill, or as a container that ended without saying why.
///
/// The agent exits 0 if and only if it wrote its report. A report found
/// beside any other exit status was left by something else, and is not
/// believed.
#[must_use]
pub fn ending(end: &BuildEnd, report: Option<BuildReport>) -> Ending {
    match report {
        Some(report) if end.exit_code == 0 => Ending::Reported(report),
        _ if end.oom_killed => Ending::OomKilled,
        _ => Ending::Unreported {
            detail: end.detail.clone(),
        },
    }
}

/// What a lost pod is reported as. `disk_bytes` is the worker's per-build
/// limit, for naming in an eviction.
#[must_use]
pub fn lost(loss: &Loss, elapsed: Duration, disk_bytes: Option<u64>) -> CompleteReport {
    match loss {
        Loss::OverLimit { message } => {
            let mut report = ended(None, elapsed, Exit::Signal);
            report.reason = Some(disk_reason(disk_bytes, message));
            report
        }
        Loss::Evicted { message } => interrupted(format!(
            "the build pod was evicted from its node: {message}"
        )),
        Loss::DeadlineExceeded => interrupted("the build pod ran past its deadline"),
        Loss::Failed { message } => interrupted(message),
    }
}

/// Why a build failed for disk, naming the limit when the worker set one.
fn disk_reason(disk_bytes: Option<u64>, message: &str) -> String {
    match disk_bytes {
        Some(bytes) => format!(
            "out of disk: the build passed its ephemeral-storage limit of {:.1} GiB \
             ({DISK_VARIABLE}) and was evicted: {message}",
            gib(bytes)
        ),
        None => format!(
            "out of disk: the build passed a storage limit the worker did not set and was \
             evicted: {message}"
        ),
    }
}

fn gib(bytes: u64) -> f64 {
    // A display figure: the precision lost past 2^53 bytes is not shown.
    #[allow(clippy::cast_precision_loss)]
    let bytes = bytes as f64;
    bytes / f64::from(1u32 << 30)
}

/// The report for a pod the cluster would not create.
#[must_use]
pub fn refused(message: &str) -> CompleteReport {
    interrupted(format!("the cluster refused the build pod: {message}"))
}

/// The report for a pod whose build did not run within `waited` of its
/// creation. `last` is the pod's last status line, `None` if the watch never
/// showed the pod; what holds it is often only in the namespace's events.
#[must_use]
pub fn unstarted(waited: Duration, last: Option<&str>, namespace: &str) -> CompleteReport {
    let last = last
        .unwrap_or("the watch never showed the pod")
        .trim_end_matches('.');
    interrupted(format!(
        "the build pod did not start within {} ({SCHEDULE_VARIABLE}); when last seen, \
         {last}. `kubectl -n {namespace} get events` says what holds it",
        aurcache_common::units::describe_duration(waited.as_secs())
    ))
}

/// The report for a pod that is no longer there.
#[must_use]
pub fn gone() -> CompleteReport {
    interrupted("the build pod disappeared: its node was lost, or it was deleted")
}

#[cfg(test)]
#[path = "../tests/support/status.rs"]
mod status;

#[cfg(test)]
mod tests {
    use super::status;
    use super::*;
    use aurcache_build_agent::report::Ended;
    use aurcache_common::worker::BuildOutcome;
    use aurcache_worker_container::outcome::complete;
    use k8s_openapi::api::core::v1::PodSpec;

    const GIB: u64 = 1 << 30;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A pod on node `n1` with this status.
    fn on_node(status: PodStatus) -> Pod {
        Pod {
            spec: Some(PodSpec {
                node_name: Some("n1".to_string()),
                ..PodSpec::default()
            }),
            status: Some(status),
            ..Pod::default()
        }
    }

    fn with_status(change: impl FnOnce(&mut PodStatus)) -> Pod {
        let mut status = status::staged();
        change(&mut status);
        on_node(status)
    }

    fn report(ended: Ended) -> BuildReport {
        BuildReport::unmeasured(ended)
    }

    fn end(exit_code: i32, reason: &str) -> BuildEnd {
        BuildEnd {
            exit_code,
            oom_killed: reason == "OOMKilled",
            detail: format!("exit code {exit_code}, {reason}"),
        }
    }

    #[test]
    fn a_pod_without_a_node_is_unscheduled_with_the_schedulers_message() {
        assert_eq!(
            observe(&Pod::default()),
            Observed::Unscheduled {
                unschedulable: None
            }
        );
        let pod = Pod {
            status: Some(status::unschedulable(
                "0/3 nodes are available: 3 Insufficient cpu.",
            )),
            ..Pod::default()
        };
        assert_eq!(
            unschedulable(&pod).as_deref(),
            Some("0/3 nodes are available: 3 Insufficient cpu.")
        );
        let line = observe(&pod).line().unwrap();
        assert!(line.contains("3 Insufficient cpu"), "{line}");
    }

    #[test]
    fn the_stages_of_a_healthy_pod() {
        let starting = on_node(status::starting("PodInitializing"));
        assert_eq!(
            observe(&starting),
            Observed::Starting {
                node: Some("n1".to_string()),
                waiting: Some("PodInitializing".to_string())
            }
        );
        assert_eq!(
            observe(&starting).line().unwrap(),
            "the build pod is starting on node n1 (PodInitializing)"
        );
        assert_eq!(observe(&on_node(status::staged())), Observed::Staged);
        assert_eq!(observe(&on_node(status::building())), Observed::Running);
        assert_eq!(
            observe(&on_node(status::built(0, "Completed"))),
            Observed::Built(end(0, "Completed"))
        );
        assert_eq!(
            observe(&on_node(status::built(137, "OOMKilled"))),
            Observed::Built(end(137, "OOMKilled"))
        );
    }

    /// The agent exits 0 exactly when it wrote its report, so a report beside
    /// any other status is someone else's and decides nothing; the kubelet's
    /// out-of-memory kill then does.
    #[test]
    fn a_report_decides_only_beside_a_clean_exit() {
        let clean = report(Ended::Exited { code: 0 });
        let judged = ending(&end(0, "Completed"), Some(clean.clone()));
        assert_eq!(judged, Ending::Reported(clean.clone()));
        assert_eq!(
            complete(None, secs(5), judged, None).outcome,
            BuildOutcome::Succeeded
        );
        let killed = ending(&end(137, "OOMKilled"), Some(clean.clone()));
        assert_eq!(killed, Ending::OomKilled);
        assert_eq!(
            complete(None, secs(5), killed, None).outcome,
            BuildOutcome::Failed
        );
        let done = complete(None, secs(5), ending(&end(1, "Error"), Some(clean)), None);
        assert_eq!(done.outcome, BuildOutcome::Interrupted);
        assert!(done.reason.unwrap().contains("Error"));
    }

    /// The row "pod `reason: Evicted`, for passing its own limit".
    #[test]
    fn an_eviction_for_its_own_limit_fails_the_build() {
        for message in [
            "Usage of EmptyDir volume \"work\" exceeds the limit \"30Gi\". ",
            "Container build exceeded its local ephemeral storage limit \"30Gi\". ",
            "Pod ephemeral local storage usage exceeds the total limit of containers 30Gi. ",
        ] {
            let Observed::Lost(loss) = observe(&on_node(status::evicted(message))) else {
                panic!("{message}");
            };
            assert!(matches!(loss, Loss::OverLimit { .. }), "{message}");
            let done = lost(&loss, secs(9), Some(30 * GIB));
            assert_eq!(done.outcome, BuildOutcome::Failed, "{message}");
            assert_eq!(
                done.reason.unwrap(),
                format!(
                    "out of disk: the build passed its ephemeral-storage limit of 30.0 GiB \
                     (WORKER_BUILD_DISK_MAX) and was evicted: {message}"
                )
            );
        }
    }

    /// The row "pod `reason: Evicted`, for anything else".
    #[test]
    fn an_eviction_for_the_nodes_sake_is_an_interruption() {
        let evicted = status::evicted(
            "The node was low on resource: ephemeral-storage. Threshold quantity: 10%, \
             available: 4%. Container build was using 9Gi, request is 0, has larger \
             consumption of ephemeral-storage. ",
        );
        let Observed::Lost(loss) = observe(&on_node(evicted)) else {
            panic!();
        };
        assert!(matches!(loss, Loss::Evicted { .. }));
        let done = lost(&loss, secs(9), Some(30 * GIB));
        assert_eq!(done.outcome, BuildOutcome::Interrupted);
        assert!(done.reason.unwrap().contains("low on resource"));
    }

    /// The row "pod `reason: DeadlineExceeded`".
    #[test]
    fn a_pod_past_its_deadline_is_interrupted() {
        let late = with_status(|status| {
            status.phase = Some("Failed".to_string());
            status.reason = Some("DeadlineExceeded".to_string());
        });
        assert_eq!(observe(&late), Observed::Lost(Loss::DeadlineExceeded));
        let done = lost(&Loss::DeadlineExceeded, secs(1), None);
        assert_eq!(done.outcome, BuildOutcome::Interrupted);
    }

    /// A container that cannot start is waited for: what the kubelet says of
    /// it is in the build log, and in the report once the start deadline
    /// passes.
    #[test]
    fn a_container_that_cannot_start_says_why_and_points_at_the_events() {
        let line = observe(&on_node(status::stage_image_missing()))
            .line()
            .unwrap();
        assert_eq!(
            line,
            "the build pod is starting on node n1 (ErrImagePull: manifest unknown)"
        );
        let done = unstarted(secs(600), Some(&line), "aurcache-builds");
        assert_eq!(done.outcome, BuildOutcome::Interrupted);
        assert_eq!(
            done.reason.as_deref(),
            Some(
                "the build pod did not start within 10m (WORKER_KUBE_SCHEDULE_TIMEOUT); when \
                 last seen, the build pod is starting on node n1 (ErrImagePull: manifest \
                 unknown). `kubectl -n aurcache-builds get events` says what holds it"
            )
        );
        assert_eq!(
            observe(&on_node(status::builder_image_missing()))
                .line()
                .unwrap(),
            "the build container is starting (ImagePullBackOff: Back-off pulling image)"
        );
    }

    #[test]
    fn a_failed_stage_hold_or_pod_is_lost() {
        let mut stage_failed = status::scheduled(status::terminated(1, "Error"), Vec::new());
        stage_failed.phase = Some("Failed".to_string());
        let Observed::Lost(Loss::Failed { message }) = observe(&on_node(stage_failed)) else {
            panic!();
        };
        assert_eq!(message, "the stage container failed: exit code 1, Error");

        let hold_died = status::scheduled(
            status::terminated(0, "Completed"),
            vec![
                status::named(BUILD, status::running()),
                status::named(HOLD, status::terminated(137, "OOMKilled")),
            ],
        );
        assert!(matches!(
            observe(&on_node(hold_died)),
            Observed::Lost(Loss::Failed { .. })
        ));

        let over = with_status(|status| status.phase = Some("Failed".to_string()));
        let Observed::Lost(loss) = observe(&over) else {
            panic!();
        };
        assert_eq!(
            lost(&loss, secs(1), None).outcome,
            BuildOutcome::Interrupted
        );
    }

    #[test]
    fn a_refused_or_vanished_pod_is_an_interruption() {
        let no = refused("exceeded quota: builds");
        assert_eq!(no.outcome, BuildOutcome::Interrupted);
        assert_eq!(
            no.reason.as_deref(),
            Some("the cluster refused the build pod: exceeded quota: builds")
        );
        assert_eq!(gone().outcome, BuildOutcome::Interrupted);
        let soon = unstarted(secs(90), None, "b");
        assert!(soon.reason.unwrap().contains(
            "within 90s (WORKER_KUBE_SCHEDULE_TIMEOUT); when last seen, the watch never showed"
        ));
    }
}
