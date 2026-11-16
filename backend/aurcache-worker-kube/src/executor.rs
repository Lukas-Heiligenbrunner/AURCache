//! The lifecycle of one build: a claimed job becomes a pod, a build log,
//! streamed packages and one report.
//!
//! `kubernetes-executor.md` describes it, from "Shape" to "How a build ended".

use crate::cluster::Cluster;
use crate::config::Config;
use anyhow::Result;
use aurcache_common::worker::CompleteReport;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::executor::{ClaimedJob, Executor};
use aurcache_worker_core::live::Live;
use std::sync::Arc;

// WP9 writes the lifecycle that reads these fields.
#[allow(dead_code)]
pub struct KubeExecutor<C: Cluster> {
    cfg: Live<Config>,
    cluster: Arc<C>,
    agent_image: String,
}

impl<C: Cluster> KubeExecutor<C> {
    pub fn new(cfg: Config, cluster: Arc<C>, agent_image: String) -> Self {
        Self {
            cfg: Live::new(cfg),
            cluster,
            agent_image,
        }
    }
}

impl<C: Cluster> Executor for KubeExecutor<C> {
    const KIND: &'static str = "kubernetes";

    fn run_job(
        &self,
        client: Arc<WorkerClient>,
        job: ClaimedJob,
    ) -> impl Future<Output = Result<CompleteReport>> + Send {
        // WP9 writes this.
        let _ = (client, job);
        async { unimplemented!() }
    }

    fn describe_self(&self) -> String {
        // WP9 writes this.
        unimplemented!()
    }
}
