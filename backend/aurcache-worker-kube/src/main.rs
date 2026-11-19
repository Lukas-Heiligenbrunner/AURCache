//! `aurcache-worker-kube`: the Kubernetes worker's dispatcher.
//!
//! Enrolls and claims jobs like every worker, and builds each one in a pod of
//! its own. In order: the configuration, the cluster, the image build pods
//! take the agent from, the pods a previous run left, enrollment, and the
//! runner.
//!
//! Told to stop (SIGTERM, SIGINT), it claims nothing more, stops the builds
//! it holds, reports each to the server as interrupted, and deletes whatever
//! pod of its own is left. Told twice, it stops at once.

use anyhow::{Context, Result};
use aurcache_worker_core::enroll;
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::runner::Runner;
use aurcache_worker_core::shutdown::{self, StopSignals, WoundDown};
use aurcache_worker_kube::cluster::Cluster;
use aurcache_worker_kube::config::{self, AgentImage, Config};
use aurcache_worker_kube::executor::KubeExecutor;
use aurcache_worker_kube::kube_cluster::KubeCluster;
use std::sync::Arc;
use std::time::Duration;

const KIND: &str = KubeExecutor::<KubeCluster>::KIND;
/// How long the builds held when this worker is told to stop get to be
/// stopped and reported. Under the Deployment's termination grace period, so
/// that the pods are deleted before the kubelet kills this process.
const WIND_DOWN: Duration = Duration::from_secs(40);

/// The worker, enrolled and ready to claim.
struct Started {
    runner: Arc<Runner<KubeExecutor<KubeCluster>>>,
    cluster: Arc<KubeCluster>,
}

#[tokio::main]
async fn main() -> Result<()> {
    aurcache_worker_core::init_tracing();
    // The Kubernetes client takes rustls' process-wide provider, and this
    // binary links two, so neither is the default until one is installed.
    // An error only says one already is.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    // Before anything else: this is the first process of its container, where
    // a signal nobody listens for is dropped, and enrolling can wait for a
    // long time.
    let mut signals = StopSignals::listen()?;

    let started = tokio::select! {
        started = start() => started?,
        signal = signals.received() => {
            tracing::info!("Received {signal} while starting: stopping");
            return Ok(());
        }
    };
    let wound_down =
        shutdown::run_until_told_to_stop(started.runner, &mut signals, WIND_DOWN).await?;
    if wound_down == WoundDown::Hurried {
        return Ok(());
    }
    leave(&*started.cluster).await
}

/// Everything before the first claim: the configuration, the cluster, the
/// pods a previous run left, and enrollment.
async fn start() -> Result<Started> {
    let core = config::core_from_env();
    aurcache_worker_container::arch::check(&core.arches)?;
    let identity = Identity::load_or_create(&core.data_dir)?;
    let cfg = Config::from_env(&core.settings, &identity.fingerprint)?;
    if let Some(files) = &cfg.credential {
        // Once now, so that a key ssh-agent will not take stops the worker
        // here and not its first build.
        let credential = files.load().await?;
        tracing::info!(
            "Builds fetch their sources with the SSH key {} ({}). It stays in this pod, in an \
             ssh-agent started for each build: a build can have it sign while it runs, and \
             never holds it",
            files.key.display(),
            credential.agent.identity()
        );
    }

    let cluster = Arc::new(KubeCluster::connect(&cfg.namespace, &cfg.dispatcher).await?);
    let agent_image = agent_image(&cluster, &cfg).await?;
    tracing::info!(
        "Build pods go to namespace {} and take the build agent from {agent_image}",
        cfg.namespace
    );
    if !cfg.user_namespaces {
        tracing::warn!(
            "Build pods share their node's user namespace: root in a build container is root \
             on its node. Set WORKER_KUBE_USER_NAMESPACES=true where the nodes support it"
        );
    }

    let executor = KubeExecutor::new(cfg.clone(), Arc::clone(&cluster), agent_image.clone());
    // Before enrolling: the first heartbeat gives up every build a previous
    // run held, and their pods are nobody's from then on.
    executor.sweep().await?;

    // Waits out a server that is not up yet; see `enroll::enroll`.
    let client = enroll::enroll(&core, &identity, KIND).await?;
    let runner = Runner::new(core, client, executor);
    Ok(Started { runner, cluster })
}

/// Delete every pod of this worker's that is still there as it stops: one
/// whose build did not get as far as deleting it.
async fn leave<C: Cluster>(cluster: &C) -> Result<()> {
    let left = cluster
        .sweep()
        .await
        .context("deleting the pods left as this worker stops")?;
    if left > 0 {
        tracing::info!("Deleted {left} pod(s) still there as this worker stopped");
    }
    Ok(())
}

/// The image the `stage` and `hold` containers run: the one named, or the
/// one this dispatcher's own pod runs, by digest, so the agent in a build pod
/// is this worker's own build.
async fn agent_image(cluster: &KubeCluster, cfg: &Config) -> Result<String> {
    match &cfg.agent_image {
        AgentImage::Given(image) => Ok(image.clone()),
        AgentImage::OwnPod { namespace, name } => cluster
            .image_of(namespace, name)
            .await
            .context("finding the image this worker's own pod runs, for the build agent"),
    }
}
