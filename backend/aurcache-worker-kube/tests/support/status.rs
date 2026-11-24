//! Build pod statuses, for the lifecycle tests and for outcome.rs's unit
//! tests. The module that includes it must have `BUILD`, `HOLD` and `STAGE`
//! in scope.

use super::{BUILD, HOLD, STAGE};
use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
    ContainerStatus, PodCondition, PodStatus,
};

pub fn running() -> ContainerState {
    ContainerState {
        running: Some(ContainerStateRunning::default()),
        ..ContainerState::default()
    }
}

pub fn waiting(reason: &str, message: &str) -> ContainerState {
    ContainerState {
        waiting: Some(ContainerStateWaiting {
            reason: Some(reason.to_string()),
            message: Some(message.to_string()),
        }),
        ..ContainerState::default()
    }
}

pub fn terminated(exit_code: i32, reason: &str) -> ContainerState {
    ContainerState {
        terminated: Some(ContainerStateTerminated {
            exit_code,
            reason: Some(reason.to_string()),
            ..ContainerStateTerminated::default()
        }),
        ..ContainerState::default()
    }
}

pub fn named(name: &str, state: ContainerState) -> ContainerStatus {
    ContainerStatus {
        name: name.to_string(),
        state: Some(state),
        ..ContainerStatus::default()
    }
}

pub fn scheduled(stage: ContainerState, rest: Vec<ContainerStatus>) -> PodStatus {
    PodStatus {
        phase: Some("Running".to_string()),
        conditions: Some(vec![PodCondition {
            type_: "PodScheduled".to_string(),
            status: "True".to_string(),
            ..PodCondition::default()
        }]),
        init_container_statuses: Some(vec![named(STAGE, stage)]),
        container_statuses: Some(rest),
        ..PodStatus::default()
    }
}

/// No node can take the pod, and the scheduler says why.
pub fn unschedulable(message: &str) -> PodStatus {
    PodStatus {
        phase: Some("Pending".to_string()),
        conditions: Some(vec![PodCondition {
            type_: "PodScheduled".to_string(),
            status: "False".to_string(),
            reason: Some("Unschedulable".to_string()),
            message: Some(message.to_string()),
            ..PodCondition::default()
        }]),
        ..PodStatus::default()
    }
}

/// On a node, with `stage` not started, and the kubelet's word for why.
pub fn starting(reason: &str) -> PodStatus {
    scheduled(waiting(reason, ""), Vec::new())
}

/// On a node, `stage` running: waiting for the job.
pub fn staged() -> PodStatus {
    scheduled(running(), Vec::new())
}

/// The agent image cannot be pulled.
pub fn stage_image_missing() -> PodStatus {
    scheduled(waiting("ErrImagePull", "manifest unknown"), Vec::new())
}

/// The build container and `hold` both run.
pub fn building() -> PodStatus {
    scheduled(
        terminated(0, "Completed"),
        vec![named(BUILD, running()), named(HOLD, running())],
    )
}

/// The builder image cannot be pulled, after the job went in.
pub fn builder_image_missing() -> PodStatus {
    scheduled(
        terminated(0, "Completed"),
        vec![
            named(BUILD, waiting("ImagePullBackOff", "Back-off pulling image")),
            named(HOLD, running()),
        ],
    )
}

/// The build container ended; `hold` runs on.
pub fn built(exit_code: i32, reason: &str) -> PodStatus {
    scheduled(
        terminated(0, "Completed"),
        vec![
            named(BUILD, terminated(exit_code, reason)),
            named(HOLD, running()),
        ],
    )
}

/// The kubelet evicted the pod, saying `message`.
pub fn evicted(message: &str) -> PodStatus {
    PodStatus {
        phase: Some("Failed".to_string()),
        reason: Some("Evicted".to_string()),
        message: Some(message.to_string()),
        ..PodStatus::default()
    }
}
