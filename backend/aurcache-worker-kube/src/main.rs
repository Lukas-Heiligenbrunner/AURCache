//! `aurcache-worker-kube`: the Kubernetes worker's dispatcher.
//!
//! Enrolls and claims jobs like every worker, and builds each one in a pod of
//! its own. In order: the configuration, the cluster, the image build pods
//! take the agent from, the pods a previous run left, enrollment, and the
//! runner. The check that the builds' NetworkPolicy is enforced runs beside
//! the runner, from once enrolled, and holds nothing up.
//!
//! Told to stop (SIGTERM, SIGINT), it claims nothing more, stops the builds
//! it holds, reports each to the server as interrupted, and deletes whatever
//! pod of its own is left.

use anyhow::{Context, Result};
use aurcache_common::api::activity::Severity;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::config::CoreConfig;
use aurcache_worker_core::enroll;
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::repo::{Endpoint, RepoSection};
use aurcache_worker_core::runner::Runner;
use aurcache_worker_kube::cluster::Cluster;
use aurcache_worker_kube::config::{self, AgentImage, Config};
use aurcache_worker_kube::executor::KubeExecutor;
use aurcache_worker_kube::kube_cluster::KubeCluster;
use aurcache_worker_kube::probe::{self, ProbeOutcome, Targets};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::task::{AbortHandle, JoinHandle};

const KIND: &str = KubeExecutor::<KubeCluster>::KIND;
/// How long the builds held when this worker is told to stop get to be
/// stopped and reported. Under the Deployment's termination grace period, so
/// that the pods are deleted before the kubelet kills this process.
const WIND_DOWN: Duration = Duration::from_secs(40);

/// The worker, enrolled and ready to claim.
struct Started {
    runner: Arc<Runner<KubeExecutor<KubeCluster>>>,
    cluster: Arc<KubeCluster>,
    /// The NetworkPolicy check, when one was started.
    probe: Option<AbortHandle>,
}

/// The signals that ask this process to stop.
struct StopSignals {
    terminate: Signal,
    interrupt: Signal,
}

impl StopSignals {
    fn listen() -> Result<Self> {
        Ok(Self {
            terminate: signal(SignalKind::terminate()).context("listening for SIGTERM")?,
            interrupt: signal(SignalKind::interrupt()).context("listening for SIGINT")?,
        })
    }

    /// The name of the first of them to arrive.
    async fn received(&mut self) -> &'static str {
        tokio::select! {
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.interrupt.recv() => "SIGINT",
        }
    }
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
    let running = Arc::clone(&started.runner).run();
    tokio::pin!(running);
    let signal = tokio::select! {
        ended = &mut running => return ended,
        signal = signals.received() => signal,
    };
    tracing::info!(
        "Received {signal}: claiming no more builds and stopping the ones running, which \
         the server queues again"
    );
    started.runner.stop();
    match tokio::time::timeout(WIND_DOWN, running).await {
        Ok(ended) => ended?,
        Err(_) => tracing::warn!(
            "Not every build was stopped and reported within {}s; the server ends the rest \
             when it stops hearing of them",
            WIND_DOWN.as_secs()
        ),
    }
    if let Some(probe) = started.probe {
        probe.abort();
    }
    leave(&*started.cluster).await
}

/// Everything before the first claim: the configuration, the cluster, the
/// pods a previous run left, enrollment, and the NetworkPolicy check started.
async fn start() -> Result<Started> {
    let core = config::core_from_env();
    let identity = Identity::load_or_create(&core.data_dir)?;
    let cfg = Config::from_env(&core.settings, &identity.fingerprint)?;

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
    // run held, and their pods are nobody's from then on. Before the probe
    // too, whose pod carries the same label.
    executor.sweep().await?;

    // Waits out a server that is not up yet; see `enroll::enroll`.
    let client = enroll::enroll(&core, &identity, KIND).await?;
    let targets = targets(&core, client.repo());
    let runner = Runner::new(core, client, executor);
    // Once enrolled, the server's worker port is known to answer, so a probe
    // that cannot reach it learns something about the policy, not about a
    // server still starting.
    let probe = cfg.network_probe.then(|| {
        let probe = tokio::spawn(network_probe(
            Arc::clone(&cluster),
            cfg,
            agent_image,
            targets,
        ));
        let handle = probe.abort_handle();
        tokio::spawn(tell_server(probe, runner.client()));
        handle
    });
    Ok(Started {
        runner,
        cluster,
        probe,
    })
}

/// Delete every pod of this worker's that is still there as it stops: one
/// whose build did not get as far as deleting it, and the NetworkPolicy
/// check's.
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

/// What the probe tries: the server's worker port, and the repository as
/// this worker's builds are told to reach it, when they are given one.
fn targets(core: &CoreConfig, repo: Option<&RepoSection>) -> Option<Targets> {
    Some(Targets {
        worker: dialled(&core.aurcache_url)?,
        repo: repo
            .and_then(RepoSection::endpoint)
            .map(|endpoint| address(&endpoint)),
    })
}

/// The `host:port` a URL is dialled on.
fn dialled(raw: &str) -> Option<String> {
    Endpoint::of(&url::Url::parse(raw).ok()?).map(|endpoint| address(&endpoint))
}

/// An endpoint as `host:port`, an IPv6 address in its brackets.
fn address(endpoint: &Endpoint) -> String {
    let Endpoint { host, port } = endpoint;
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Find out whether the builds' NetworkPolicy is enforced, and say so in
/// this worker's log. `Some` is what the server should hear as well.
async fn network_probe(
    cluster: Arc<KubeCluster>,
    cfg: Config,
    agent_image: String,
    targets: Option<Targets>,
) -> Option<String> {
    let Some(targets) = targets else {
        tracing::warn!(
            "The builds' NetworkPolicy was not checked: AURCACHE_URL names no host and port \
             to try"
        );
        return None;
    };
    let outcome = probe::check(&*cluster, &cfg, &agent_image, &targets).await;
    let unenforced = unenforced(&outcome);
    match &outcome {
        ProbeOutcome::Enforced if targets.repo.is_some() => tracing::info!(
            "The builds' NetworkPolicy is enforced: a pod labelled like a build reached the \
             repository, and not the server's worker port, the cluster's API or the \
             metadata address"
        ),
        ProbeOutcome::Enforced => tracing::info!(
            "The builds' NetworkPolicy looks enforced: a pod labelled like a build could not \
             reach the server's worker port, the cluster's API or the metadata address. \
             Builds are given no repository to try, so nothing told this from a pod that \
             reaches nothing"
        ),
        ProbeOutcome::Open { .. } => {
            tracing::error!("{}", unenforced.as_deref().unwrap_or_default());
        }
        ProbeOutcome::Inconclusive { reason } => tracing::warn!(
            "Could not check whether the builds' NetworkPolicy is enforced: {reason}"
        ),
    }
    unenforced
}

/// The sentence for a policy that is not enforced; `None` for any other
/// outcome.
fn unenforced(outcome: &ProbeOutcome) -> Option<String> {
    let ProbeOutcome::Open { reachable } = outcome else {
        return None;
    };
    Some(format!(
        "The builds' NetworkPolicy is not enforced: {} answered from a pod labelled like a \
         build. The cluster's CNI may not enforce NetworkPolicy. Builds can reach the \
         server's worker port and the cluster",
        reachable.join(", ")
    ))
}

/// Tell the server what the probe found, when that is a policy not enforced:
/// at error severity, so it shows on the worker's page and not only in this
/// process's log.
async fn tell_server(probe: JoinHandle<Option<String>>, reporter: Arc<WorkerClient>) {
    let Ok(Some(message)) = probe.await else {
        return;
    };
    if let Err(error) = reporter
        .report_problem(Severity::Error, None, &message)
        .await
    {
        tracing::debug!("reporting the network probe's result failed: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_is_dialled_on_its_port_or_its_schemes() {
        assert_eq!(
            dialled("https://aurcache.aurcache.svc:8083").as_deref(),
            Some("aurcache.aurcache.svc:8083")
        );
        assert_eq!(
            dialled("http://repo.example/$arch").as_deref(),
            Some("repo.example:80")
        );
        assert_eq!(
            dialled("https://[fd00::1]/").as_deref(),
            Some("[fd00::1]:443")
        );
        assert_eq!(dialled("not a url"), None);
    }

    #[test]
    fn only_an_open_policy_is_for_the_server() {
        assert_eq!(unenforced(&ProbeOutcome::Enforced), None);
        let inconclusive = ProbeOutcome::Inconclusive {
            reason: "the probe pod was not running within 120s".to_string(),
        };
        assert_eq!(unenforced(&inconclusive), None);
        let open = ProbeOutcome::Open {
            reachable: vec![
                "10.0.0.5:8083".to_string(),
                "169.254.169.254:80".to_string(),
            ],
        };
        assert_eq!(
            unenforced(&open).unwrap(),
            "The builds' NetworkPolicy is not enforced: 10.0.0.5:8083, 169.254.169.254:80 \
             answered from a pod labelled like a build. The cluster's CNI may not enforce \
             NetworkPolicy. Builds can reach the server's worker port and the cluster"
        );
    }
}
