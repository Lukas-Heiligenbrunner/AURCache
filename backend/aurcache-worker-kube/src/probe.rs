//! Whether the builds' NetworkPolicy is enforced, found out at startup.
//!
//! A pod with the build pods' labels, so the policy selects it, tries the
//! server's worker port, the cluster's API and the cloud metadata address, and
//! the repository when there is one. The cluster's API cannot say whether its
//! CNI enforces policies; this asks the network instead.
//!
//! The repository is the one target a build is allowed, and so the control:
//! where it does not answer either, nothing was learned, because a pod that
//! reaches nothing at all gives the same answers as one behind an enforced
//! policy.
//!
//! The pod only holds still; the agent's `probe` runs in it by exec. A CNI
//! may take a moment after a pod starts to put a policy around it, so an
//! answer that says something is open is asked for again, a few times, before
//! it is believed.

use crate::cluster::{Cluster, CreateError, ExecEnd, Stdin};
use crate::config::Config;
use crate::pod;
use anyhow::{Context, Result, bail};
use aurcache_build_agent::report::Reach;
use k8s_openapi::api::core::v1::{
    Capabilities, Container, Pod, PodSecurityContext, PodSpec, SeccompProfile, SecurityContext,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// What the probe tries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Targets {
    /// `host:port` of the server's worker port.
    pub worker: String,
    /// `host:port` of the repository, when there is one.
    pub repo: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// What must be closed is closed, and the repository, where one was
    /// tried, answered.
    Enforced,
    /// These answered and should not have.
    Open { reachable: Vec<String> },
    /// The probe could not be run or read, or what it found says nothing
    /// either way.
    Inconclusive { reason: String },
}

/// The cloud metadata address, where providers serve a node's credentials.
const METADATA: &str = "169.254.169.254:80";
/// The probe container's name.
const PROBE: &str = "probe";
/// How long each connection may take, in seconds.
const CONNECT_SECONDS: u64 = 5;
/// The pod's own deadline, in seconds.
const DEADLINE_SECONDS: i64 = 60;
/// How long to wait for the pod to finish, pulling its image included.
const WAIT: Duration = Duration::from_secs(120);
/// How long the pod gets to go away.
const GRACE: Duration = Duration::ZERO;
/// The most of the probe's answer that is read, in bytes.
const ANSWER_MAX: u64 = 64 * 1024;
/// How many times an answer that finds something open is checked again.
const RECHECKS: usize = 3;
/// How long to wait before checking again.
const SETTLE: Duration = Duration::from_secs(4);
const NOBODY: i64 = 65534;

/// Run the probe pod, read what it found, and delete it.
///
/// The cluster's own API address is probed too when this process runs in a
/// pod, which is where `KUBERNETES_SERVICE_HOST` comes from.
pub async fn check<C: Cluster>(
    cluster: &C,
    cfg: &Config,
    agent_image: &str,
    targets: &Targets,
) -> ProbeOutcome {
    run(cluster, cfg, agent_image, targets, api_target(), WAIT).await
}

/// `host:port` of the cluster's API as the environment of a pod in the
/// cluster names it, when this process is in one.
fn api_target() -> Option<String> {
    let host = std::env::var("KUBERNETES_SERVICE_HOST").ok()?;
    let port = std::env::var("KUBERNETES_SERVICE_PORT").ok()?;
    Some(if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    })
}

/// What the probe is asked about.
struct Asked {
    /// Targets that must not answer.
    closed: Vec<String>,
    /// The target that should.
    repo: Option<String>,
}

/// The probe, with the cluster's API address and the wait given.
async fn run<C: Cluster>(
    cluster: &C,
    cfg: &Config,
    agent_image: &str,
    targets: &Targets,
    api: Option<String>,
    wait: Duration,
) -> ProbeOutcome {
    let name = format!("aurcache-probe-{}", cfg.dispatcher);
    let mut closed = vec![targets.worker.clone(), METADATA.to_string()];
    closed.extend(api);
    let asked = Asked {
        closed,
        repo: targets.repo.clone(),
    };
    let outcome = match attempt(cluster, cfg, agent_image, &name, &asked, wait).await {
        Ok(reach) => {
            let outcome = judge(&asked, &reach);
            // An inconclusive outcome says it in its reason.
            if let (ProbeOutcome::Open { .. }, Some(repo)) = (&outcome, silent_repo(&asked, &reach))
            {
                tracing::warn!(
                    "the network probe could not reach the repository at {repo}; \
                     builds will not find AURCache's own packages"
                );
            }
            outcome
        }
        Err(err) => ProbeOutcome::Inconclusive {
            reason: format!("{err:#}"),
        },
    };
    if let Err(err) = cluster.delete(&name, GRACE).await {
        tracing::warn!("deleting the network probe pod {name}: {err:#}");
    }
    outcome
}

/// Start the pod, and ask it until the answer is one to believe.
async fn attempt<C: Cluster>(
    cluster: &C,
    cfg: &Config,
    agent_image: &str,
    name: &str,
    asked: &Asked,
    wait: Duration,
) -> Result<Vec<Reach>> {
    let mut named = asked.closed.clone();
    let mut connect = Vec::new();
    for target in &asked.closed {
        let address = resolved(target)
            .await
            .with_context(|| format!("cannot address {target} for the network probe"))?;
        connect.push(address);
    }
    if let Some(repo) = &asked.repo {
        match resolved(repo).await {
            Ok(address) => {
                connect.push(address);
                named.push(repo.clone());
            }
            Err(err) => tracing::warn!("not probing the repository {repo}: {err:#}"),
        }
    }
    // A pod left by an earlier run would make the name taken.
    cluster.delete(name, GRACE).await?;
    let pod = render(cfg, agent_image, name);
    match cluster.create(&pod).await {
        Ok(()) => {}
        Err(CreateError::Refused { message }) => {
            bail!("the cluster refused the probe pod: {message}")
        }
        Err(CreateError::Exists) => bail!("a pod named {name} exists and would not go away"),
        Err(CreateError::Other(err)) => return Err(err.context("creating the probe pod")),
    }
    running(cluster, name, wait).await?;
    let mut reach = ask(cluster, name, &connect, &named).await?;
    for _ in 0..RECHECKS {
        if matches!(judge(asked, &reach), ProbeOutcome::Enforced) {
            break;
        }
        tokio::time::sleep(SETTLE).await;
        reach = ask(cluster, name, &connect, &named).await?;
    }
    Ok(reach)
}

/// `host:port` as `ip:port`, looked up here: a minimal image's resolver is not
/// to be trusted, and a name that fails to resolve there reads as closed.
async fn resolved(target: &str) -> Result<String> {
    if target.parse::<SocketAddr>().is_ok() {
        return Ok(target.to_string());
    }
    let mut found = tokio::net::lookup_host(target)
        .await
        .with_context(|| format!("looking up {target}"))?;
    let address = found.next().context("the name has no address")?;
    Ok(address.to_string())
}

/// Wait until the pod's container is running.
async fn running<C: Cluster>(cluster: &C, name: &str, wait: Duration) -> Result<()> {
    let mut moved = cluster.generation();
    let watching = async {
        loop {
            let phase = cluster
                .pod(name)
                .and_then(|pod| pod.status.as_ref().and_then(|status| status.phase.clone()));
            match phase.as_deref() {
                Some("Running") => return Ok(()),
                Some("Succeeded" | "Failed") => bail!("the probe pod ended before it was asked"),
                _ => {}
            }
            if moved.changed().await.is_err() {
                bail!("the watch on pods ended");
            }
        }
    };
    match tokio::time::timeout(wait, watching).await {
        Ok(done) => done,
        Err(_) => bail!("the probe pod was not running within {}s", wait.as_secs()),
    }
}

/// Run the agent's `probe` in the pod and read what it printed.
async fn ask<C: Cluster>(
    cluster: &C,
    name: &str,
    connect: &[String],
    named: &[String],
) -> Result<Vec<Reach>> {
    let agent = pod::agent_path();
    let timeout = CONNECT_SECONDS.to_string();
    let mut argv = vec![agent.as_str(), "probe", "--timeout", timeout.as_str()];
    for target in connect {
        argv.push("--connect");
        argv.push(target);
    }
    let exec = cluster.exec(name, PROBE, &argv, Stdin::Closed).await?;
    let mut text = String::new();
    exec.stdout
        .take(ANSWER_MAX)
        .read_to_string(&mut text)
        .await
        .context("reading the probe's answer")?;
    match exec.ended.await {
        ExecEnd::Succeeded => {}
        ExecEnd::Failed { message } | ExecEnd::Lost { message } => {
            bail!("the probe did not run: {message}")
        }
    }
    read(&text, named)
}

/// The probe's answer, under the names the caller gave: the last line it
/// printed, which lists targets in the order they were asked.
fn read(text: &str, named: &[String]) -> Result<Vec<Reach>> {
    let line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .context("the probe printed nothing")?;
    let reach: Vec<Reach> =
        serde_json::from_str(line).context("the probe's answer is not readable")?;
    if reach.len() != named.len() {
        bail!(
            "the probe answered for {} targets and was asked about {}",
            reach.len(),
            named.len()
        );
    }
    Ok(reach
        .into_iter()
        .zip(named)
        .map(|(answer, target)| Reach {
            target: target.clone(),
            reachable: answer.reachable,
        })
        .collect())
}

/// What the answers say.
fn judge(asked: &Asked, reach: &[Reach]) -> ProbeOutcome {
    let open: Vec<String> = reach
        .iter()
        .filter(|answer| answer.reachable && asked.closed.contains(&answer.target))
        .map(|answer| answer.target.clone())
        .collect();
    if !open.is_empty() {
        return ProbeOutcome::Open { reachable: open };
    }
    match silent_repo(asked, reach) {
        None => ProbeOutcome::Enforced,
        Some(repo) => ProbeOutcome::Inconclusive {
            reason: format!(
                "nothing answered a pod labelled like a build, the repository at {repo} \
                 included, which builds are allowed to reach. A pod that reaches nothing at \
                 all looks the same as an enforced policy; and builds that cannot reach the \
                 repository will not find AURCache's own packages"
            ),
        },
    }
}

/// The repository, when it was asked about and did not answer.
fn silent_repo<'a>(asked: &'a Asked, reach: &[Reach]) -> Option<&'a str> {
    let repo = asked.repo.as_deref()?;
    let reached = reach
        .iter()
        .any(|answer| answer.target == repo && answer.reachable);
    (!reached).then_some(repo)
}

/// The probe pod, which only holds still: it carries the build pods' labels, and nothing else of theirs.
fn render(cfg: &Config, agent_image: &str, name: &str) -> Pod {
    let extras = &cfg.extras;
    let mut labels = extras.labels.clone().unwrap_or_default();
    labels.extend(pod::labels(&cfg.dispatcher));
    let mut node_selector = extras.node_selector.clone().unwrap_or_default();
    node_selector.insert("kubernetes.io/os".to_string(), "linux".to_string());
    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(cfg.namespace.clone()),
            labels: Some(labels),
            annotations: extras.annotations.clone(),
            ..ObjectMeta::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".to_string()),
            active_deadline_seconds: Some(DEADLINE_SECONDS),
            service_account_name: Some(pod::SERVICE_ACCOUNT.to_string()),
            automount_service_account_token: Some(false),
            enable_service_links: Some(false),
            node_selector: Some(node_selector),
            security_context: Some(PodSecurityContext {
                seccomp_profile: Some(SeccompProfile {
                    type_: "RuntimeDefault".to_string(),
                    localhost_profile: None,
                }),
                ..PodSecurityContext::default()
            }),
            containers: vec![Container {
                name: PROBE.to_string(),
                image: Some(agent_image.to_string()),
                command: Some(vec![pod::agent_path(), "hold".to_string()]),
                security_context: Some(restricted()),
                ..Container::default()
            }],
            tolerations: extras.tolerations.clone(),
            affinity: extras.affinity.clone(),
            priority_class_name: extras.priority_class_name.clone(),
            runtime_class_name: extras.runtime_class_name.clone(),
            image_pull_secrets: extras.image_pull_secrets.clone(),
            scheduler_name: extras.scheduler_name.clone(),
            ..PodSpec::default()
        }),
        ..Pod::default()
    }
}

/// The `restricted` Pod Security level's container settings, as nobody.
fn restricted() -> SecurityContext {
    SecurityContext {
        run_as_user: Some(NOBODY),
        run_as_group: Some(NOBODY),
        run_as_non_root: Some(true),
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: None,
        }),
        ..SecurityContext::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{Admission, Exec};
    use crate::config::{AgentImage, PodExtras, Tunables};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tokio::sync::watch;

    /// What the fake cluster does with the probe pod.
    enum Behaviour {
        /// Running; each exec prints the next text, and the last repeats.
        Answers(Vec<String>),
        /// Running, with an exec that ends in failure.
        ExecFails,
        /// Failed at once.
        Fails,
        /// Pending for ever.
        Hangs,
        /// Refused at creation.
        Refused,
    }

    struct Fake {
        run: Behaviour,
        asked: Mutex<VecDeque<Vec<String>>>,
        created: Mutex<Vec<Pod>>,
        deleted: Mutex<Vec<String>>,
        pods: Mutex<Vec<Arc<Pod>>>,
        generation: watch::Sender<u64>,
    }

    impl Fake {
        fn new(run: Behaviour) -> Self {
            Self {
                run,
                asked: Mutex::default(),
                created: Mutex::default(),
                deleted: Mutex::default(),
                pods: Mutex::default(),
                generation: watch::channel(0).0,
            }
        }
    }

    impl Cluster for Fake {
        async fn create(&self, pod: &Pod) -> Result<(), CreateError> {
            let phase = match self.run {
                Behaviour::Refused => {
                    return Err(CreateError::Refused {
                        message: "quota".into(),
                    });
                }
                Behaviour::Fails => "Failed",
                Behaviour::Hangs => "Pending",
                _ => "Running",
            };
            self.created.lock().unwrap().push(pod.clone());
            let mut shown = pod.clone();
            shown.status = Some(k8s_openapi::api::core::v1::PodStatus {
                phase: Some(phase.to_string()),
                ..Default::default()
            });
            self.pods.lock().unwrap().push(Arc::new(shown));
            self.generation.send_modify(|n| *n += 1);
            Ok(())
        }
        async fn admits(&self, _: &Pod) -> Admission {
            Admission::Admitted
        }
        async fn delete(&self, name: &str, _: Duration) -> Result<()> {
            self.deleted.lock().unwrap().push(name.to_string());
            Ok(())
        }
        async fn sweep(&self) -> Result<usize> {
            Ok(0)
        }
        fn pod(&self, name: &str) -> Option<Arc<Pod>> {
            let pods = self.pods.lock().unwrap();
            pods.iter()
                .find(|pod| pod.metadata.name.as_deref() == Some(name))
                .cloned()
        }
        fn pods(&self) -> Vec<Arc<Pod>> {
            self.pods.lock().unwrap().clone()
        }
        fn generation(&self) -> watch::Receiver<u64> {
            self.generation.subscribe()
        }
        async fn exec(&self, _: &str, container: &str, argv: &[&str], _: Stdin) -> Result<Exec> {
            assert_eq!(container, PROBE);
            let mut asked = self.asked.lock().unwrap();
            let call = asked.len();
            asked.push_back(argv.iter().map(|word| (*word).to_string()).collect());
            let (text, end) = match &self.run {
                Behaviour::Answers(all) => {
                    (all[call.min(all.len() - 1)].clone(), ExecEnd::Succeeded)
                }
                _ => (
                    String::new(),
                    ExecEnd::Failed {
                        message: "exit 1".into(),
                    },
                ),
            };
            Ok(Exec {
                stdin: None,
                stdout: Box::pin(std::io::Cursor::new(text.into_bytes())),
                ended: Box::pin(async move { end }),
            })
        }
        async fn log_tail(&self, _: &str, _: &str, _: i64) -> Result<String> {
            bail!("the probe is not read from its log")
        }
    }

    fn config() -> Config {
        Config {
            namespace: "aurcache-builds".into(),
            dispatcher: "abcdef0123456789".into(),
            builder_image: "reg/builder:1".into(),
            pull_policy: "Always".into(),
            agent_image: AgentImage::Given("reg/agent@sha256:1".into()),
            credential_secret: None,
            user_namespaces: false,
            extras: PodExtras::default(),
            network_probe: true,
            tunables: Tunables {
                memory: None,
                cpus: None,
                disk: None,
                schedule_timeout: Duration::from_secs(600),
                keyserver: "hkps://k".into(),
            },
        }
    }

    fn targets(repo: bool) -> Targets {
        Targets {
            worker: "10.1.2.3:8083".into(),
            repo: repo.then(|| "10.1.2.3:80".into()),
        }
    }

    /// The probe's answer for the targets it was asked about, in order (the
    /// worker, the metadata address, then the cluster's API or the
    /// repository), as `reached` says.
    fn said<const N: usize>(reached: [bool; N]) -> String {
        let list: Vec<Reach> = reached
            .into_iter()
            .enumerate()
            .map(|(at, reachable)| Reach {
                target: format!("x:{at}"),
                reachable,
            })
            .collect();
        format!("{}\n", serde_json::to_string(&list).unwrap())
    }

    async fn probe(fake: &Fake, repo: bool, api: Option<&str>) -> ProbeOutcome {
        run(
            fake,
            &config(),
            "reg/agent@sha256:1",
            &targets(repo),
            api.map(String::from),
            Duration::from_secs(5),
        )
        .await
    }

    fn inconclusive(outcome: ProbeOutcome) -> String {
        match outcome {
            ProbeOutcome::Inconclusive { reason } => reason,
            other => panic!("expected inconclusive, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn closed_targets_mean_enforced() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, true])]));
        assert_eq!(probe(&fake, true, None).await, ProbeOutcome::Enforced);
        assert_eq!(fake.asked.lock().unwrap().len(), 1);
    }

    /// Nothing answering is what an enforced policy and a network that lets
    /// nothing through have in common.
    #[tokio::test(start_paused = true)]
    async fn a_repository_that_does_not_answer_either_is_inconclusive() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, false])]));
        let reason = inconclusive(probe(&fake, true, None).await);
        assert!(reason.contains("the repository at 10.1.2.3:80"), "{reason}");
        // It may only be slow to answer a new pod, so it is asked again.
        assert_eq!(fake.asked.lock().unwrap().len(), 1 + RECHECKS);
    }

    #[tokio::test(start_paused = true)]
    async fn a_repository_that_answers_late_is_enforced() {
        let fake = Fake::new(Behaviour::Answers(vec![
            said([false, false, false]),
            said([false, false, true]),
        ]));
        assert_eq!(probe(&fake, true, None).await, ProbeOutcome::Enforced);
    }

    /// With no repository to try there is no control, and closed is all
    /// that can be said.
    #[tokio::test]
    async fn closed_targets_without_a_repository_mean_enforced() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false])]));
        assert_eq!(probe(&fake, false, None).await, ProbeOutcome::Enforced);
    }

    /// Something open is open whatever the repository does.
    #[tokio::test(start_paused = true)]
    async fn an_open_port_beside_a_silent_repository_is_open() {
        let fake = Fake::new(Behaviour::Answers(vec![said([true, false, false])]));
        assert_eq!(
            probe(&fake, true, None).await,
            ProbeOutcome::Open {
                reachable: vec!["10.1.2.3:8083".into()]
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_answering_worker_port_is_open() {
        let fake = Fake::new(Behaviour::Answers(vec![said([true, false, true])]));
        assert_eq!(
            probe(&fake, true, None).await,
            ProbeOutcome::Open {
                reachable: vec!["10.1.2.3:8083".into()]
            }
        );
        // The answer is checked again before it is believed.
        assert_eq!(fake.asked.lock().unwrap().len(), 1 + RECHECKS);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answering_metadata_address_is_open() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, true, true])]));
        assert_eq!(
            probe(&fake, true, None).await,
            ProbeOutcome::Open {
                reachable: vec![METADATA.into()]
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_that_settles_is_enforced() {
        let fake = Fake::new(Behaviour::Answers(vec![
            said([true, true, true]),
            said([false, false, true]),
        ]));
        assert_eq!(probe(&fake, true, None).await, ProbeOutcome::Enforced);
        assert_eq!(fake.asked.lock().unwrap().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn the_clusters_api_is_asked_about_too() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, true])]));
        let outcome = probe(&fake, false, Some("10.96.0.1:443")).await;
        assert_eq!(
            outcome,
            ProbeOutcome::Open {
                reachable: vec!["10.96.0.1:443".into()]
            }
        );
        let asked = fake.asked.lock().unwrap();
        assert_eq!(
            asked[0][1..],
            [
                "probe",
                "--timeout",
                "5",
                "--connect",
                "10.1.2.3:8083",
                "--connect",
                METADATA,
                "--connect",
                "10.96.0.1:443"
            ]
        );
    }

    #[tokio::test]
    async fn the_pod_carries_the_build_labels_and_is_deleted() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, true])]));
        probe(&fake, true, None).await;
        let created = fake.created.lock().unwrap();
        assert_eq!(created.len(), 1);
        let pod = &created[0];
        let labels = pod.metadata.labels.as_ref().unwrap();
        assert_eq!(
            labels
                .get("app.kubernetes.io/managed-by")
                .map(String::as_str),
            Some("aurcache-worker-kube")
        );
        assert_eq!(
            labels.get("aurcache.io/dispatcher").map(String::as_str),
            Some("abcdef0123456789")
        );
        let spec = pod.spec.as_ref().unwrap();
        assert_eq!(spec.active_deadline_seconds, Some(60));
        let container = &spec.containers[0];
        assert_eq!(container.name, PROBE);
        assert_eq!(container.image.as_deref(), Some("reg/agent@sha256:1"));
        let security = container.security_context.as_ref().unwrap();
        assert_eq!(security.allow_privilege_escalation, Some(false));
        assert_eq!(security.run_as_non_root, Some(true));
        let name = pod.metadata.name.clone().unwrap();
        assert_eq!(fake.deleted.lock().unwrap().last(), Some(&name));
    }

    #[tokio::test]
    async fn a_failed_pod_is_inconclusive_and_deleted() {
        let fake = Fake::new(Behaviour::Fails);
        inconclusive(probe(&fake, true, None).await);
        assert!(!fake.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_pod_that_never_runs_is_inconclusive_and_deleted() {
        let fake = Fake::new(Behaviour::Hangs);
        let reason = inconclusive(probe(&fake, true, None).await);
        assert!(reason.contains("not running"), "{reason}");
        assert!(fake.deleted.lock().unwrap().len() >= 2);
    }

    #[tokio::test]
    async fn a_refused_pod_is_inconclusive() {
        let fake = Fake::new(Behaviour::Refused);
        let reason = inconclusive(probe(&fake, true, None).await);
        assert!(reason.contains("quota"), "{reason}");
        assert!(!fake.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_exec_that_fails_is_inconclusive_and_deleted() {
        let fake = Fake::new(Behaviour::ExecFails);
        let reason = inconclusive(probe(&fake, true, None).await);
        assert!(reason.contains("did not run"), "{reason}");
        assert!(!fake.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unreadable_output_is_inconclusive() {
        for text in ["", "\n", "not json\n", "[]\n"] {
            let fake = Fake::new(Behaviour::Answers(vec![text.into()]));
            inconclusive(probe(&fake, true, None).await);
            assert!(!fake.deleted.lock().unwrap().is_empty());
        }
    }

    /// Against a cluster: `AURCACHE_KUBE_TESTS=1`, `KUBECONFIG`,
    /// `AURCACHE_KUBE_TEST_NAMESPACE` (where the probe runs),
    /// `AURCACHE_PROBE_IMAGE` (an image with the agent at
    /// `/usr/local/bin`), `AURCACHE_PROBE_WORKER` and `AURCACHE_PROBE_REPO`
    /// (`ip:port`), `AURCACHE_PROBE_API` and `AURCACHE_PROBE_EXPECT`
    /// (`enforced` or `open`).
    #[tokio::test]
    #[ignore = "needs a cluster; see the variables above"]
    async fn live() {
        if std::env::var("AURCACHE_KUBE_TESTS").is_err() {
            return;
        }
        let var = |name: &str| std::env::var(name).unwrap();
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut cfg = config();
        cfg.namespace = var("AURCACHE_KUBE_TEST_NAMESPACE");
        cfg.dispatcher = "probelive".into();
        let cluster = crate::kube_cluster::KubeCluster::connect(&cfg.namespace, &cfg.dispatcher)
            .await
            .unwrap();
        let targets = Targets {
            worker: var("AURCACHE_PROBE_WORKER"),
            repo: Some(var("AURCACHE_PROBE_REPO")),
        };
        let outcome = run(
            &cluster,
            &cfg,
            &var("AURCACHE_PROBE_IMAGE"),
            &targets,
            std::env::var("AURCACHE_PROBE_API").ok(),
            Duration::from_secs(120),
        )
        .await;
        eprintln!("probe outcome: {outcome:?}");
        match var("AURCACHE_PROBE_EXPECT").as_str() {
            "enforced" => assert_eq!(outcome, ProbeOutcome::Enforced),
            _ => assert!(matches!(outcome, ProbeOutcome::Open { .. }), "{outcome:?}"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(cluster.pod("aurcache-probe-probelive").is_none());
    }
}
