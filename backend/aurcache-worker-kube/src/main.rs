//! `aurcache-worker-kube`: the Kubernetes worker's dispatcher.
//!
//! Enrolls and claims jobs like every worker, and builds each one in a pod of
//! its own. In order: the configuration, the cluster, the image build pods
//! take the agent from, the pods a previous run left, enrollment, and the
//! runner. The check that the builds' NetworkPolicy is enforced runs beside
//! all of that, from before enrollment, and holds nothing up.

use anyhow::{Context, Result};
use aurcache_common::api::activity::Severity;
use aurcache_worker_core::client::{ClientIdentity, WorkerClient};
use aurcache_worker_core::config::CoreConfig;
use aurcache_worker_core::enroll;
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::repo::Endpoint;
use aurcache_worker_core::runner::Runner;
use aurcache_worker_kube::config::{self, AgentImage, Config};
use aurcache_worker_kube::executor::KubeExecutor;
use aurcache_worker_kube::kube_cluster::KubeCluster;
use aurcache_worker_kube::probe::{self, ProbeOutcome, Targets};
use std::sync::Arc;
use tokio::task::JoinHandle;

const KIND: &str = KubeExecutor::<KubeCluster>::KIND;

#[tokio::main]
async fn main() -> Result<()> {
    aurcache_worker_core::init_tracing();
    // The Kubernetes client takes rustls' process-wide provider, and this
    // binary links two, so neither is the default until one is installed.
    // An error only says one already is.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

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

    let probe = cfg
        .network_probe
        .then(|| tokio::spawn(network_probe(cluster, cfg, agent_image, targets(&core))));

    // Waits out a server that is not up yet; see `enroll::enroll`.
    let client = enroll::enroll(&core, &identity, KIND).await?;
    if let Some(probe) = probe {
        let reporter = reporter(&core, &identity, &client)?;
        tokio::spawn(tell_server(probe, reporter));
    }

    Runner::new(core, client, executor).run().await
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

/// What the probe tries: the server's worker port, and the repository where
/// this worker is told its address. The address the server would give is
/// only known once enrolled, which the probe does not wait for.
fn targets(core: &CoreConfig) -> Option<Targets> {
    Some(Targets {
        worker: dialled(&core.aurcache_url)?,
        repo: core.repo_url.as_deref().and_then(dialled),
    })
}

/// The `host:port` a URL is dialled on.
fn dialled(raw: &str) -> Option<String> {
    let Endpoint { host, port } = Endpoint::of(&url::Url::parse(raw).ok()?)?;
    Some(if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    })
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
        ProbeOutcome::Enforced => tracing::info!(
            "The builds' NetworkPolicy is enforced: a pod labelled like a build could not \
             reach the server's worker port, the cluster's API or the metadata address"
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

/// A client of this worker's identity for the probe's report: the runner
/// keeps the one enrollment returned to itself.
fn reporter(core: &CoreConfig, identity: &Identity, client: &WorkerClient) -> Result<WorkerClient> {
    let csr = client
        .registered_csr()
        .context("the enrolled client holds no registration")?;
    let lease = client
        .lease()
        .context("the enrolled client holds no lease")?;
    WorkerClient::authenticated(
        &core.aurcache_url,
        &ClientIdentity {
            ca_pem: &identity.ca_pem()?,
            cert_pem: &identity.cert_pem()?,
            key_pem: &identity.key_pem(),
        },
        enroll::register_request(core, csr, KIND),
        lease,
        None,
    )
}

/// Tell the server what the probe found, when that is a policy not enforced:
/// at error severity, so it shows on the worker's page and not only in this
/// process's log.
async fn tell_server(probe: JoinHandle<Option<String>>, reporter: WorkerClient) {
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
