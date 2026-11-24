//! How a build pod is doing and how it ended, from its status and the
//! hand-back, as pure functions.
//!
//! [`observe`] reads where a pod is from its status. A pod whose build
//! container ended is judged by [`ending`], from what the container's status
//! and the agent's report say; a pod that can no longer hand anything back is
//! reported by [`lost`]. The other ways a build ends without a pod to read
//! have a report each: [`refused`], [`unscheduled_past`], [`unstarted_past`],
//! [`gone`].

use crate::pod::{BUILD, HOLD, STAGE};
use aurcache_build_agent::report::BuildReport;
use aurcache_common::worker::CompleteReport;
use aurcache_worker_container::outcome::Ending;
use aurcache_worker_core::protocol::{Stop, Timeout};
use aurcache_worker_core::report::{Exit, ended, interrupted};
use k8s_openapi::api::core::v1::{ContainerState, ContainerStatus, Pod, PodStatus};
use std::time::Duration;

/// The variable an operator raises the per-build disk limit with.
const DISK_VARIABLE: &str = "WORKER_BUILD_DISK_MAX";
/// The variable that says how long a pod may wait for a node, and then to
/// start on it.
const SCHEDULE_VARIABLE: &str = "WORKER_KUBE_SCHEDULE_TIMEOUT";

/// Reasons a container waits that waiting does not cure: the image has a
/// name nothing can pull, or the container cannot be made from the pod's own
/// description.
const STUCK: [&str; 4] = [
    "InvalidImageName",
    "ErrImageNeverPull",
    "CreateContainerConfigError",
    "CreateContainerError",
];

/// Reasons a container waits that may pass: a pull failed, and the kubelet
/// tries again.
const UNPULLED: [&str; 2] = ["ErrImagePull", "ImagePullBackOff"];

/// Where a build pod is, as far as its status says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observed {
    /// No node has taken it. `unschedulable` is the scheduler's message once
    /// it has looked and found none.
    Unscheduled { unschedulable: Option<String> },
    /// On a node, with `stage` not running yet.
    Starting {
        node: Option<String>,
        waiting: Option<String>,
    },
    /// A container's image could not be pulled, and the node tries again.
    /// Whoever reads this decides how long that is worth waiting for.
    Unpulled { message: String },
    /// `stage` runs: the pod is waiting for the job.
    Staged,
    /// The job is in, and the build container has not ended.
    Building {
        /// Whether `hold` runs, which is what the log is followed through.
        hold_runs: bool,
        waiting: Option<String>,
    },
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
    /// The status in a few words, for a report that has nothing better: an
    /// out-of-memory kill shows here as `OOMKilled`.
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
    /// A container cannot start, and waiting does not cure it.
    Stuck { message: String },
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
    if let Some(loss) = pod_loss(status) {
        return Observed::Lost(loss);
    }
    let node = pod.spec.as_ref().and_then(|spec| spec.node_name.clone());
    if let Some(unschedulable) = unscheduled(status, node.as_deref()) {
        return Observed::Unscheduled { unschedulable };
    }
    let stage = container(status.init_container_statuses.as_deref(), STAGE);
    match stage.map(state) {
        Some(State::Running) => return Observed::Staged,
        Some(State::Ended { exit_code: 0, .. }) => {}
        Some(State::Ended { detail, .. }) => {
            return Observed::Lost(Loss::Failed {
                message: format!("the stage container failed: {detail}"),
            });
        }
        Some(State::Waiting { reason, message }) if is_stuck(reason.as_deref()) => {
            return Observed::Lost(Loss::Stuck {
                message: stuck_message(STAGE, reason.as_deref(), message.as_deref()),
            });
        }
        Some(State::Waiting { reason, message }) if is_unpulled(reason.as_deref()) => {
            return Observed::Unpulled {
                message: stuck_message(STAGE, reason.as_deref(), message.as_deref()),
            };
        }
        Some(State::Waiting { reason, .. }) => {
            return Observed::Starting {
                node,
                waiting: reason,
            };
        }
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
        exit_code, detail, ..
    }) = &build
    {
        return Observed::Built(BuildEnd {
            exit_code: *exit_code,
            detail: detail.clone(),
        });
    }
    for (name, found) in [(BUILD, &build), (HOLD, &hold)] {
        if let Some(State::Waiting { reason, message }) = found
            && is_stuck(reason.as_deref())
        {
            return Observed::Lost(Loss::Stuck {
                message: stuck_message(name, reason.as_deref(), message.as_deref()),
            });
        }
    }
    if let Some(State::Ended { detail, .. }) = &hold {
        return Observed::Lost(Loss::Failed {
            message: format!("the hold container ended while the build ran: {detail}"),
        });
    }
    for (name, found) in [(BUILD, &build), (HOLD, &hold)] {
        if let Some(State::Waiting { reason, message }) = found
            && is_unpulled(reason.as_deref())
        {
            return Observed::Unpulled {
                message: stuck_message(name, reason.as_deref(), message.as_deref()),
            };
        }
    }
    let waiting = match build {
        Some(State::Waiting { reason, .. }) => reason,
        _ => None,
    };
    Observed::Building {
        hold_runs: matches!(hold, Some(State::Running)),
        waiting,
    }
}

/// What the pod's own phase and reason say is beyond recovery, before any
/// container is looked at: an evicted pod's containers are all gone, `hold`
/// included.
fn pod_loss(status: &PodStatus) -> Option<Loss> {
    let message = status.message.clone().unwrap_or_default();
    match status.reason.as_deref() {
        Some("Evicted") if over_own_limit(&message) => return Some(Loss::OverLimit { message }),
        Some("Evicted") => return Some(Loss::Evicted { message }),
        Some("DeadlineExceeded") => return Some(Loss::DeadlineExceeded),
        _ => {}
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
/// kubelet words those as a limit that was exceeded ("Usage of EmptyDir
/// volume "work" exceeds the limit", "Container build exceeded its local
/// ephemeral storage limit", "Pod ephemeral local storage usage exceeds the
/// total limit of containers"), and an eviction for the node's sake as "The
/// node was low on resource".
fn over_own_limit(message: &str) -> bool {
    message.contains("exceed") && message.contains("limit")
}

/// `Some` while no node has taken the pod, holding the scheduler's message
/// when it found none that can.
fn unscheduled(status: &PodStatus, node: Option<&str>) -> Option<Option<String>> {
    let scheduled = status
        .conditions
        .iter()
        .flatten()
        .find(|condition| condition.type_ == "PodScheduled");
    match scheduled {
        Some(condition) if condition.status == "True" => None,
        Some(condition) if condition.reason.as_deref() == Some("Unschedulable") => Some(Some(
            condition
                .message
                .clone()
                .unwrap_or_else(|| "no node can take it".to_string()),
        )),
        Some(_) => Some(None),
        None if node.is_some() => None,
        None => Some(None),
    }
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
    Waiting {
        reason: Option<String>,
        message: Option<String>,
    },
    Running,
    Ended {
        exit_code: i32,
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
            detail,
        }
    } else if running.is_some() {
        State::Running
    } else if let Some(waiting) = waiting {
        State::Waiting {
            reason: waiting.reason.clone(),
            message: waiting.message.clone(),
        }
    } else {
        State::Unknown
    }
}

fn is_stuck(reason: Option<&str>) -> bool {
    reason.is_some_and(|reason| STUCK.contains(&reason))
}

fn is_unpulled(reason: Option<&str>) -> bool {
    reason.is_some_and(|reason| UNPULLED.contains(&reason))
}

fn stuck_message(container: &str, reason: Option<&str>, message: Option<&str>) -> String {
    let reason = reason.unwrap_or("it cannot start");
    match message.filter(|message| !message.is_empty()) {
        Some(message) => format!("the {container} container cannot start: {reason}: {message}"),
        None => format!("the {container} container cannot start: {reason}"),
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
                ..
            } => Some(format!("the build container is starting ({reason})")),
            Self::Building { waiting: None, .. } => Some("the build is running".to_string()),
            Self::Unpulled { message } => Some(format!("{message}; the node tries again")),
            Self::Staged | Self::Built(_) | Self::Lost(_) => None,
        }
    }
}

/// How a build container that ended on its own is to be reported: by the
/// agent's report when it wrote one, and otherwise as a container that ended
/// without saying why.
///
/// The agent exits 0 if and only if it wrote its report. A report found
/// beside any other exit status was left by something else, and is not
/// believed.
#[must_use]
pub fn ending(end: &BuildEnd, report: Option<BuildReport>) -> Ending {
    match report {
        Some(report) if end.exit_code == 0 => Ending::Reported(report),
        _ => Ending::Unreported {
            detail: end.detail.clone(),
        },
    }
}

/// What a lost pod is reported as. `timeout` is the build's own, for a pod
/// that ran into its deadline before the dispatcher stopped it; `disk` the
/// worker's per-build limit, for naming in an eviction.
#[must_use]
pub fn lost(
    loss: &Loss,
    elapsed: Duration,
    timeout: Option<Timeout>,
    disk: Option<u64>,
) -> CompleteReport {
    match loss {
        Loss::OverLimit { message } => {
            let mut report = ended(None, elapsed, Exit::Signal);
            report.reason = Some(disk_reason(disk, message));
            report
        }
        Loss::Evicted { message } => interrupted(format!(
            "the build pod was evicted from its node: {message}"
        )),
        Loss::DeadlineExceeded => match timeout {
            Some(timeout) => ended(Some(Stop::TimedOut(timeout)), elapsed, Exit::Unknown),
            None => interrupted("the build pod ran past its deadline"),
        },
        Loss::Stuck { message } | Loss::Failed { message } => interrupted(message),
    }
}

/// Why a build failed for disk, naming the limit when the worker set one.
fn disk_reason(disk: Option<u64>, message: &str) -> String {
    match disk {
        Some(bytes) => format!(
            "out of disk: the build passed its ephemeral-storage limit of {:.1} GiB \
             ({DISK_VARIABLE}) and was evicted",
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

/// The report for a pod no node took within `waited`, with the scheduler's
/// message when it gave one.
#[must_use]
pub fn unscheduled_past(waited: Duration, message: Option<&str>) -> CompleteReport {
    let waited = with_unit(waited);
    let why = message.map_or_else(String::new, |message| format!(": {message}"));
    interrupted(format!(
        "no node could take the build within {waited} ({SCHEDULE_VARIABLE}){why}"
    ))
}

/// The report for a pod that a node took, and that did not start one of
/// its containers within `waited`. `last` is the last its status said of
/// it, which seldom says why: that is in the namespace's events.
#[must_use]
pub fn unstarted_past(waited: Duration, last: &str, namespace: &str) -> CompleteReport {
    interrupted(format!(
        "the build pod did not start within {} ({SCHEDULE_VARIABLE}); when last seen, \
         {last}. `kubectl -n {namespace} get events` says what holds it: an image that \
         cannot be pulled, a volume that cannot be mounted, or a network the node cannot \
         give the pod",
        with_unit(waited)
    ))
}

/// The report for a hand-in that did not end within `waited`.
#[must_use]
pub fn unreceived_past(waited: Duration) -> CompleteReport {
    interrupted(format!(
        "the build pod did not take the job within {} ({SCHEDULE_VARIABLE})",
        with_unit(waited)
    ))
}

/// `waited` as `format_duration` gives it, with an `s` on a bare count of
/// seconds, which has no unit of its own.
fn with_unit(waited: Duration) -> String {
    let mut shown = aurcache_common::units::format_duration(waited.as_secs());
    if shown.bytes().all(|byte| byte.is_ascii_digit()) {
        shown.push('s');
    }
    shown
}

/// The report for a pod that is no longer there.
#[must_use]
pub fn gone() -> CompleteReport {
    interrupted("the build pod disappeared: its node was lost, or it was deleted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_build_agent::report::Ended;
    use aurcache_common::worker::BuildOutcome;
    use aurcache_worker_container::outcome::complete;
    use aurcache_worker_core::protocol::TimeoutSource;
    use k8s_openapi::api::core::v1::{
        ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting, PodCondition,
        PodSpec,
    };

    const GIB: u64 = 1 << 30;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn running() -> ContainerState {
        ContainerState {
            running: Some(ContainerStateRunning::default()),
            ..ContainerState::default()
        }
    }

    fn waiting(reason: &str, message: &str) -> ContainerState {
        ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some(reason.to_string()),
                message: Some(message.to_string()),
            }),
            ..ContainerState::default()
        }
    }

    fn terminated(exit_code: i32, reason: &str) -> ContainerState {
        ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code,
                reason: Some(reason.to_string()),
                ..ContainerStateTerminated::default()
            }),
            ..ContainerState::default()
        }
    }

    fn named(name: &str, state: ContainerState) -> ContainerStatus {
        ContainerStatus {
            name: name.to_string(),
            state: Some(state),
            ..ContainerStatus::default()
        }
    }

    fn scheduled() -> PodCondition {
        PodCondition {
            type_: "PodScheduled".to_string(),
            status: "True".to_string(),
            ..PodCondition::default()
        }
    }

    /// A pod on node `n1` with these container states.
    fn pod(stage: ContainerState, rest: &[(&str, ContainerState)]) -> Pod {
        Pod {
            spec: Some(PodSpec {
                node_name: Some("n1".to_string()),
                ..PodSpec::default()
            }),
            status: Some(PodStatus {
                phase: Some("Running".to_string()),
                conditions: Some(vec![scheduled()]),
                init_container_statuses: Some(vec![named(STAGE, stage)]),
                container_statuses: Some(
                    rest.iter()
                        .map(|(name, state)| named(name, state.clone()))
                        .collect(),
                ),
                ..PodStatus::default()
            }),
            ..Pod::default()
        }
    }

    fn with_status(change: impl FnOnce(&mut PodStatus)) -> Pod {
        let mut pod = pod(running(), &[]);
        change(pod.status.as_mut().unwrap());
        pod
    }

    fn report(ended: Ended) -> BuildReport {
        BuildReport::unmeasured(ended)
    }

    fn end(exit_code: i32, reason: &str) -> BuildEnd {
        BuildEnd {
            exit_code,
            detail: format!("exit code {exit_code}, {reason}"),
        }
    }

    #[test]
    fn a_pod_with_no_status_or_no_node_is_unscheduled() {
        assert_eq!(
            observe(&Pod::default()),
            Observed::Unscheduled {
                unschedulable: None
            }
        );
        let pending = Pod {
            status: Some(PodStatus {
                phase: Some("Pending".to_string()),
                ..PodStatus::default()
            }),
            ..Pod::default()
        };
        assert_eq!(
            observe(&pending),
            Observed::Unscheduled {
                unschedulable: None
            }
        );
        assert_eq!(unschedulable(&pending), None);
    }

    #[test]
    fn the_schedulers_refusal_is_read_with_its_message() {
        let pod = Pod {
            status: Some(PodStatus {
                phase: Some("Pending".to_string()),
                conditions: Some(vec![PodCondition {
                    type_: "PodScheduled".to_string(),
                    status: "False".to_string(),
                    reason: Some("Unschedulable".to_string()),
                    message: Some("0/3 nodes are available: 3 Insufficient cpu.".to_string()),
                    ..PodCondition::default()
                }]),
                ..PodStatus::default()
            }),
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
        let starting = pod(waiting("PodInitializing", ""), &[]);
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
        assert_eq!(observe(&pod(running(), &[])), Observed::Staged);

        let pulling = pod(
            terminated(0, "Completed"),
            &[
                (BUILD, waiting("ContainerCreating", "")),
                (HOLD, waiting("ContainerCreating", "")),
            ],
        );
        assert_eq!(
            observe(&pulling),
            Observed::Building {
                hold_runs: false,
                waiting: Some("ContainerCreating".to_string())
            }
        );
        let building = pod(
            terminated(0, "Completed"),
            &[(BUILD, running()), (HOLD, running())],
        );
        assert_eq!(
            observe(&building),
            Observed::Building {
                hold_runs: true,
                waiting: None
            }
        );
        let built = pod(
            terminated(0, "Completed"),
            &[(BUILD, terminated(0, "Completed")), (HOLD, running())],
        );
        assert_eq!(observe(&built), Observed::Built(end(0, "Completed")));
    }

    /// The agent exits 0 exactly when it wrote its report, so a report beside
    /// any other status is someone else's and decides nothing.
    #[test]
    fn a_report_decides_only_beside_a_clean_exit() {
        let clean = report(Ended::Exited { code: 0 });
        let judged = ending(&end(0, "Completed"), Some(clean.clone()));
        assert_eq!(judged, Ending::Reported(clean.clone()));
        assert_eq!(
            complete(None, secs(5), judged, None).outcome,
            BuildOutcome::Succeeded
        );
        for (exit_code, reason) in [(137, "OOMKilled"), (1, "Error")] {
            let judged = ending(&end(exit_code, reason), Some(clean.clone()));
            let done = complete(None, secs(5), judged, None);
            assert_eq!(done.outcome, BuildOutcome::Interrupted);
            assert!(done.reason.unwrap().contains(reason));
        }
    }

    /// The row "pod `reason: Evicted`, for passing its own limit".
    #[test]
    fn an_eviction_for_its_own_limit_fails_the_build() {
        for message in [
            "Usage of EmptyDir volume \"work\" exceeds the limit \"30Gi\". ",
            "Container build exceeded its local ephemeral storage limit \"30Gi\". ",
            "Pod ephemeral local storage usage exceeds the total limit of containers 30Gi. ",
        ] {
            let evicted = with_status(|status| {
                status.phase = Some("Failed".to_string());
                status.reason = Some("Evicted".to_string());
                status.message = Some(message.to_string());
            });
            let Observed::Lost(loss) = observe(&evicted) else {
                panic!("{message}");
            };
            assert!(matches!(loss, Loss::OverLimit { .. }), "{message}");
            let done = lost(&loss, secs(9), None, Some(30 * GIB));
            assert_eq!(done.outcome, BuildOutcome::Failed, "{message}");
            assert_eq!(
                done.reason.as_deref(),
                Some(
                    "out of disk: the build passed its ephemeral-storage limit of 30.0 GiB \
                     (WORKER_BUILD_DISK_MAX) and was evicted"
                )
            );
        }
    }

    /// The row "pod `reason: Evicted`, for anything else".
    #[test]
    fn an_eviction_for_the_nodes_sake_is_an_interruption() {
        let evicted = with_status(|status| {
            status.phase = Some("Failed".to_string());
            status.reason = Some("Evicted".to_string());
            status.message = Some(
                "The node was low on resource: ephemeral-storage. Threshold quantity: 10%, \
                 available: 4%. Container build was using 9Gi, request is 0, has larger \
                 consumption of ephemeral-storage. "
                    .to_string(),
            );
        });
        let Observed::Lost(loss) = observe(&evicted) else {
            panic!();
        };
        assert!(matches!(loss, Loss::Evicted { .. }));
        let done = lost(&loss, secs(9), None, Some(30 * GIB));
        assert_eq!(done.outcome, BuildOutcome::Interrupted);
        assert!(done.reason.unwrap().contains("low on resource"));
    }

    /// The row "pod `reason: DeadlineExceeded`".
    #[test]
    fn a_pod_past_its_deadline_timed_out() {
        let late = with_status(|status| {
            status.phase = Some("Failed".to_string());
            status.reason = Some("DeadlineExceeded".to_string());
        });
        assert_eq!(observe(&late), Observed::Lost(Loss::DeadlineExceeded));
        let timeout = Timeout {
            limit: secs(3600),
            set_by: TimeoutSource::Server,
        };
        let done = lost(&Loss::DeadlineExceeded, secs(4200), Some(timeout), None);
        assert_eq!(done.outcome, BuildOutcome::Failed);
        assert_eq!(done.exit_code, Some(124));
        assert!(done.reason.unwrap().contains("timed out after 4200s"));
        let unlimited = lost(&Loss::DeadlineExceeded, secs(1), None, None);
        assert_eq!(unlimited.outcome, BuildOutcome::Interrupted);
    }

    /// The row "`CreateContainerConfigError`", wherever in the pod it shows:
    /// a container that cannot be made from the pod's own description.
    #[test]
    fn a_container_that_cannot_start_is_an_interruption_with_the_status_message() {
        let cases = [
            pod(
                waiting("InvalidImageName", "couldn't parse image name"),
                &[],
            ),
            pod(
                terminated(0, "Completed"),
                &[
                    (BUILD, waiting("ErrImageNeverPull", "image is not present")),
                    (HOLD, running()),
                ],
            ),
            pod(
                terminated(0, "Completed"),
                &[
                    (BUILD, running()),
                    (
                        HOLD,
                        waiting("CreateContainerConfigError", "secret not found"),
                    ),
                ],
            ),
        ];
        for case in &cases {
            let Observed::Lost(loss) = observe(case) else {
                panic!("{case:?}");
            };
            assert!(matches!(loss, Loss::Stuck { .. }));
            let done = lost(&loss, secs(1), None, None);
            assert_eq!(done.outcome, BuildOutcome::Interrupted);
            assert!(done.reason.unwrap().contains("cannot start"));
        }
    }

    /// The row "`ErrImagePull`/`ImagePullBackOff`": the node tries again, so
    /// it is not a loss by itself, in whichever container it shows.
    #[test]
    fn an_image_that_could_not_be_pulled_is_not_yet_a_loss() {
        let cases = [
            pod(waiting("ErrImagePull", "manifest unknown"), &[]),
            pod(
                terminated(0, "Completed"),
                &[
                    (BUILD, waiting("ImagePullBackOff", "Back-off pulling image")),
                    (HOLD, running()),
                ],
            ),
        ];
        for case in &cases {
            assert!(
                matches!(observe(case), Observed::Unpulled { .. }),
                "{case:?}"
            );
        }
        let observed = observe(&cases[0]);
        assert_eq!(
            observed.line().unwrap(),
            "the stage container cannot start: ErrImagePull: manifest unknown; the node tries \
             again"
        );
    }

    /// The row "a pod that does not start": the worker's, with where to look.
    #[test]
    fn a_pod_that_does_not_start_is_an_interruption_that_points_at_the_events() {
        let done = unstarted_past(
            secs(600),
            "the build pod is starting on node n1 (PodInitializing)",
            "aurcache-builds",
        );
        assert_eq!(done.outcome, BuildOutcome::Interrupted);
        let reason = done.reason.unwrap();
        assert!(reason.contains("did not start within 10m"), "{reason}");
        assert!(reason.contains("WORKER_KUBE_SCHEDULE_TIMEOUT"), "{reason}");
        assert!(reason.contains("(PodInitializing)"), "{reason}");
        assert!(
            reason.contains("kubectl -n aurcache-builds get events"),
            "{reason}"
        );
    }

    #[test]
    fn a_pod_whose_containers_are_all_gone_has_nothing_to_hand_back() {
        let stage_failed = pod(terminated(1, "Error"), &[]);
        let Observed::Lost(Loss::Failed { message }) = observe(&stage_failed) else {
            panic!();
        };
        assert_eq!(message, "the stage container failed: exit code 1, Error");

        let hold_died = pod(
            terminated(0, "Completed"),
            &[(BUILD, running()), (HOLD, terminated(137, "OOMKilled"))],
        );
        assert!(matches!(
            observe(&hold_died),
            Observed::Lost(Loss::Failed { .. })
        ));

        let over = with_status(|status| status.phase = Some("Failed".to_string()));
        let Observed::Lost(loss) = observe(&over) else {
            panic!();
        };
        assert_eq!(
            lost(&loss, secs(1), None, None).outcome,
            BuildOutcome::Interrupted
        );
    }

    /// The rows "unschedulable past the timeout", "creation refused" and "the
    /// pod disappears".
    #[test]
    fn the_endings_with_no_pod_to_read_are_interruptions() {
        let late = unscheduled_past(secs(600), Some("0/3 nodes are available"));
        assert_eq!(late.outcome, BuildOutcome::Interrupted);
        assert_eq!(
            late.reason.as_deref(),
            Some(
                "no node could take the build within 10m (WORKER_KUBE_SCHEDULE_TIMEOUT): \
                 0/3 nodes are available"
            )
        );
        let soon = unscheduled_past(secs(90), None);
        assert_eq!(
            soon.reason.as_deref(),
            Some("no node could take the build within 90s (WORKER_KUBE_SCHEDULE_TIMEOUT)")
        );
        let no = refused("exceeded quota: builds");
        assert_eq!(no.outcome, BuildOutcome::Interrupted);
        assert_eq!(
            no.reason.as_deref(),
            Some("the cluster refused the build pod: exceeded quota: builds")
        );
        assert_eq!(gone().outcome, BuildOutcome::Interrupted);
    }

    /// A stop is the worker's own and wins over anything the pod says.
    #[test]
    fn a_stop_wins_over_a_missing_report() {
        let done = complete(
            Some(Stop::Canceled),
            secs(3),
            ending(&end(137, "Error"), None),
            None,
        );
        assert_eq!(done.outcome, BuildOutcome::Canceled);
    }
}
