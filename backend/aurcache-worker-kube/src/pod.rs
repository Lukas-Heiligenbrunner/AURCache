//! A build pod, rendered from the configuration and a job, with nothing in
//! between that needs a cluster.
//!
//! `kubernetes-executor.md` ("The build pod") is what this renders, field for
//! field.

use crate::config::Config;
use anyhow::Result;
use aurcache_common::worker::JobDescriptor;
use k8s_openapi::api::core::v1::Pod;
use std::collections::BTreeMap;
use std::time::Duration;

/// `app.kubernetes.io/managed-by` on every pod this worker creates.
pub const MANAGED_BY: &str = "aurcache-worker-kube";
/// The container that runs the build.
pub const BUILD: &str = "build";
/// The init container that installs the agent and receives the job.
pub const STAGE: &str = "stage";
/// The container that keeps the pod up for the hand-back.
pub const HOLD: &str = "hold";
/// Where the work volume is mounted, in every container.
pub const WORK: &str = "/work";

/// What a build pod is rendered from.
pub struct PodInputs<'a> {
    pub cfg: &'a Config,
    pub agent_image: &'a str,
    pub job: &'a JobDescriptor,
    pub timeout: Option<Duration>,
}

/// `aurcache-build-<dispatcher>-<build_id>`.
#[must_use]
pub fn name(dispatcher: &str, build_id: i32) -> String {
    // WP4 writes this.
    let _ = (dispatcher, build_id);
    unimplemented!()
}

/// `aurcache.io/dispatcher=<dispatcher>`.
#[must_use]
pub fn selector(dispatcher: &str) -> String {
    // WP4 writes this.
    let _ = dispatcher;
    unimplemented!()
}

/// Labels every pod of this dispatcher carries, a build's or not.
#[must_use]
pub fn labels(dispatcher: &str) -> BTreeMap<String, String> {
    // WP4 writes this.
    let _ = dispatcher;
    unimplemented!()
}

/// The pod for one build. An error for an architecture with no OCI name.
pub fn render(inputs: &PodInputs<'_>) -> Result<Pod> {
    // WP4 writes this.
    let _ = inputs;
    unimplemented!()
}
