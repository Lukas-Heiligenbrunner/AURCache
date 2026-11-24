//! `aurcache-worker-kube`: the Kubernetes worker's dispatcher.
//!
//! Enrolls and claims jobs like every worker, and builds each one in a pod of
//! its own. In order: the configuration, the cluster, the pods a previous
//! run left, a dry run of a build pod, enrollment, and the runner.
//!
//! Told to stop (SIGTERM, SIGINT), it claims nothing more, stops the builds
//! it holds, reports each to the server as interrupted, and deletes whatever
//! pod of its own is left. Told twice, it stops at once.

use anyhow::{Result, bail};
use aurcache_worker_container::arch::ARCHES;
use aurcache_worker_core::enroll;
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::runner::Runner;
use aurcache_worker_core::shutdown::{self, StopSignals, WoundDown};
use aurcache_worker_kube::cluster::Cluster;
use aurcache_worker_kube::config::{self, Config};
use aurcache_worker_kube::executor::{self, KubeExecutor};
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
/// pods a previous run left, a dry run of a build pod, and enrollment.
async fn start() -> Result<Started> {
    let core = config::core_from_env();
    if let Some(arch) = core
        .arches
        .iter()
        .find(|arch| !ARCHES.contains(&arch.as_str()))
    {
        bail!(
            "this worker does not build for {arch} (WORKER_ARCHES, or the machine's own when \
             that is unset); it builds for {}",
            ARCHES.join(", ")
        );
    }
    let identity = Identity::load_or_create(&core.data_dir)?;
    let cfg = Config::from_env(&core.settings, identity.tag())?;
    if let Some(files) = &cfg.credential {
        // Once now, so that a key that ssh-agent will not take stops the
        // worker here and not its first build.
        files.load().await?;
        tracing::info!(
            "Builds fetch their sources with the SSH key {}, as the default key; it stays \
             in this pod, in an ssh-agent started for each build",
            files.key.display()
        );
    }

    let cluster = Arc::new(KubeCluster::connect(&cfg.namespace, &cfg.dispatcher).await?);
    tracing::info!(
        "Build pods go to namespace {} and take the build agent from {}",
        cfg.namespace,
        cfg.agent_image
    );

    let executor = KubeExecutor::new(cfg, Arc::clone(&cluster));
    // Before enrolling: the first heartbeat gives up every build a previous
    // run held, and their pods are nobody's from then on.
    executor.sweep().await?;
    executor.check_admission().await?;

    // Waits out a server that is not up yet; see `enroll::enroll`.
    let client = enroll::enroll(&core, &identity, KIND).await?;
    let runner = Runner::new(core, client, executor);
    Ok(Started { runner, cluster })
}

/// Delete this worker's build pods still there as it stops: one whose build
/// did not get as far as deleting it.
async fn leave<C: Cluster>(cluster: &C) -> Result<()> {
    let left = executor::leave(cluster).await?;
    if left > 0 {
        tracing::info!("Deleted {left} pod(s) still there as this worker stopped");
    }
    Ok(())
}
