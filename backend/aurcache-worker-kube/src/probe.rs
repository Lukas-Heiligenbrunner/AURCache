//! Whether the builds' NetworkPolicy is enforced, found out at startup.
//!
//! A pod with the build pods' labels, so the policy selects it, tries the
//! server's worker port, the cluster's API and the cloud metadata address, and
//! the repository when there is one. The cluster's API cannot say whether its
//! CNI enforces policies; this asks the network instead.
//!
//! The worker port and the API are the controls. This worker dials both
//! itself while the pod is asked, so one that answers this worker and not a
//! pod the policy selects, while that pod reaches something, was shut out by
//! the policy; one that answers neither tells nothing. What answers the pod
//! and should not is something the policy lets through, which is what a
//! public address is to the policy as shipped; an address the policy as
//! shipped shuts out answering it means the policy is not enforced, whatever
//! else did not answer. Where nothing answers at all, the repository is what
//! tells an enforced policy from a pod that reaches nothing.
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
use aurcache_worker_core::repo::Endpoint;
use k8s_openapi::api::core::v1::{Container, Pod, PodSecurityContext, PodSpec, SeccompProfile};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// What the probe tries, each as `host:port`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Targets {
    /// The server's worker port.
    pub worker: String,
    /// The cluster's API, when this process runs in a pod ([`api_target`]).
    pub api: Option<String>,
    /// The repository, when builds are given one.
    pub repo: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// What must be closed is closed, and the repository answered.
    Enforced,
    /// What must be closed is closed, and there was no repository to try:
    /// a pod that reaches nothing at all would give the same answers.
    LooksEnforced,
    /// The policy is at work, since a control that answered this worker did
    /// not answer the pod, and lets builds reach these: addresses it does not
    /// shut out, such as public ones.
    LetThrough { reachable: Vec<String> },
    /// These answered, and at least one is an address the policy as shipped
    /// shuts out.
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
/// The pod's own deadline, in seconds, counted from when a node took it:
/// past the wait for it to run and every time it is asked, so that it is
/// this worker that ends it.
const DEADLINE_SECONDS: i64 = 300;
/// The most the probe uses its pod for, from its creation: its deadline.
const LIFETIME: Duration = Duration::from_secs(DEADLINE_SECONDS.unsigned_abs());
/// How long to wait for the pod to run, pulling its image included.
const WAIT: Duration = Duration::from_secs(120);
/// How long the pod gets to go away.
const GRACE: Duration = crate::cluster::PROMPT;
/// The most of the probe's answer that is read, in bytes.
const ANSWER_MAX: u64 = 64 * 1024;
/// How many times an answer that finds something open is checked again.
const RECHECKS: usize = 3;
/// How long to wait before checking again.
const SETTLE: Duration = Duration::from_secs(4);

/// How this worker tries a control itself, beside the probe pod.
trait Dial: Sync {
    /// Whether `address` accepts a connection from this worker.
    fn reaches(&self, address: SocketAddr) -> impl Future<Output = bool> + Send;
}

/// A TCP connection, with the probe's own timeout.
struct Tcp;

impl Dial for Tcp {
    async fn reaches(&self, address: SocketAddr) -> bool {
        let connect = tokio::net::TcpStream::connect(address);
        matches!(
            tokio::time::timeout(Duration::from_secs(CONNECT_SECONDS), connect).await,
            Ok(Ok(_))
        )
    }
}

/// Run the probe pod, read what it found, and delete it.
pub async fn check<C: Cluster>(cluster: &C, cfg: &Config, targets: &Targets) -> ProbeOutcome {
    run(cluster, &Tcp, cfg, &cfg.agent_image, targets, WAIT).await
}

/// Whether `pod`, one of no build, is a probe pod the probe no longer uses,
/// as of `now`: one that no node has taken past the wait for it to run, or
/// any older than the probe's use of it. Only a delete that failed, or a
/// create whose answer was lost, leaves one. Without a node it would hold
/// claims back for good, since its deadline counts only from when a node
/// takes it.
#[must_use]
pub fn abandoned(pod: &Pod, now: Timestamp) -> bool {
    let Some(created) = &pod.metadata.creation_timestamp else {
        return false;
    };
    let age = now.duration_since(created.0);
    let past = |limit: Duration| SignedDuration::try_from(limit).is_ok_and(|limit| age >= limit);
    let placed = pod
        .spec
        .as_ref()
        .is_some_and(|spec| spec.node_name.is_some());
    past(LIFETIME) || (!placed && past(WAIT))
}

/// `host:port` of the cluster's API as the environment of a pod in the
/// cluster names it, when this process is in one.
#[must_use]
pub fn api_target() -> Option<String> {
    let host = std::env::var("KUBERNETES_SERVICE_HOST").ok()?;
    let port = std::env::var("KUBERNETES_SERVICE_PORT")
        .ok()?
        .parse()
        .ok()?;
    Some(address(&Endpoint { host, port }))
}

/// An endpoint as `host:port`, an IPv6 address in its brackets.
#[must_use]
pub fn address(endpoint: &Endpoint) -> String {
    let Endpoint { host, port } = endpoint;
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// What a target is to the probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The server's worker port, which answers this worker: it enrolled
    /// through it.
    Worker,
    /// The cluster's API, which answers this worker. Its address is a
    /// Service's, so whether what is behind it is public does not show.
    Api,
    /// The cloud metadata address, where nothing may be listening.
    Metadata,
    /// The repository, the one target a build must reach.
    Repository,
}

impl Kind {
    /// Whether a build must not reach it.
    fn closed(self) -> bool {
        self != Self::Repository
    }

    /// Whether it answers this worker, so that silence towards the probe is
    /// the policy's doing.
    fn control(self) -> bool {
        matches!(self, Self::Worker | Self::Api)
    }
}

/// One target, as named and as dialled.
#[derive(Clone, Debug)]
struct Target {
    name: String,
    address: SocketAddr,
    kind: Kind,
}

/// One target, whether it answered the pod, and whether it answered this
/// worker meanwhile.
struct Answer<'a> {
    target: &'a Target,
    reachable: bool,
    /// Only a control is dialled from here; any other target is `false`.
    here: bool,
}

/// What one asking found, a flag per target in the order tried.
struct Round {
    /// Whether the target answered the probe pod.
    pod: Vec<bool>,
    /// Whether it answered this worker while the pod was asked; `false` for
    /// a target that is not a control, which is not dialled from here.
    here: Vec<bool>,
}

/// The probe, with the wait given.
async fn run<C: Cluster>(
    cluster: &C,
    dial: &impl Dial,
    cfg: &Config,
    agent_image: &str,
    targets: &Targets,
    wait: Duration,
) -> ProbeOutcome {
    let tried = match resolve(targets).await {
        Ok(tried) => tried,
        Err(err) => {
            return ProbeOutcome::Inconclusive {
                reason: format!("{err:#}"),
            };
        }
    };
    let name = match create(cluster, &render(cfg, agent_image)).await {
        Ok(name) => name,
        Err(err) => {
            return ProbeOutcome::Inconclusive {
                reason: format!("{err:#}"),
            };
        }
    };
    // Within the pod's lifetime, so that a pod that old is never in use.
    let attempted = tokio::time::timeout(LIFETIME, attempt(cluster, dial, &name, &tried, wait))
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "the probe took longer than {}s",
                LIFETIME.as_secs()
            ))
        });
    let outcome = match attempted {
        Ok(round) => {
            let answers = answers(&tried, &round);
            let outcome = judge(&answers);
            // An inconclusive outcome says it in its reason.
            if !matches!(outcome, ProbeOutcome::Inconclusive { .. })
                && let Some(repo) = silent_repo(&answers)
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

/// Each target, looked up here: a minimal image's resolver is not to be
/// trusted, and a name that fails to resolve there reads as closed. A
/// repository whose name does not resolve is left out, and said so once.
async fn resolve(targets: &Targets) -> Result<Vec<Target>> {
    let mut tried = vec![
        closed(Kind::Worker, &targets.worker).await?,
        closed(Kind::Metadata, METADATA).await?,
    ];
    if let Some(api) = &targets.api {
        tried.push(closed(Kind::Api, api).await?);
    }
    if let Some(repo) = &targets.repo {
        match resolved(repo).await {
            Ok(address) => tried.push(Target {
                name: repo.clone(),
                address,
                kind: Kind::Repository,
            }),
            Err(err) => tracing::warn!(
                "the network probe did not try the repository at {repo}, whose name does \
                 not resolve here: {err:#}"
            ),
        }
    }
    Ok(tried)
}

/// A target that must be closed, looked up; one that cannot be is an error.
async fn closed(kind: Kind, name: &str) -> Result<Target> {
    let address = resolved(name)
        .await
        .with_context(|| format!("cannot address {name} for the network probe"))?;
    Ok(Target {
        name: name.to_string(),
        address,
        kind,
    })
}

/// `host:port` as an address, looked up when it is not one.
async fn resolved(target: &str) -> Result<SocketAddr> {
    if let Ok(address) = target.parse::<SocketAddr>() {
        return Ok(address);
    }
    let mut found = tokio::net::lookup_host(target)
        .await
        .with_context(|| format!("looking up {target}"))?;
    found.next().context("the name has no address")
}

/// Create the pod, under a name the API server makes up, and return that
/// name: one left by an earlier run, still going away, cannot be in the way.
async fn create<C: Cluster>(cluster: &C, pod: &Pod) -> Result<String> {
    match cluster.create(pod).await {
        Ok(created) => created
            .metadata
            .name
            .context("the API server named no probe pod"),
        Err(CreateError::Refused { message }) => {
            bail!("the cluster refused the probe pod: {message}")
        }
        Err(CreateError::Exists) => bail!("the probe pod's generated name was taken"),
        Err(CreateError::Other(err)) => Err(err.context("creating the probe pod")),
    }
}

/// Wait for the pod, and ask it until the answer is one to believe.
async fn attempt<C: Cluster>(
    cluster: &C,
    dial: &impl Dial,
    name: &str,
    tried: &[Target],
    wait: Duration,
) -> Result<Round> {
    running(cluster, name, wait).await?;
    let mut round = asked(cluster, dial, name, tried).await?;
    for _ in 0..RECHECKS {
        let settled = judge(&answers(tried, &round));
        if matches!(
            settled,
            ProbeOutcome::Enforced | ProbeOutcome::LooksEnforced
        ) {
            break;
        }
        tokio::time::sleep(SETTLE).await;
        round = asked(cluster, dial, name, tried).await?;
    }
    Ok(round)
}

/// Ask the pod, dialling the controls from here at the same time.
async fn asked<C: Cluster>(
    cluster: &C,
    dial: &impl Dial,
    name: &str,
    tried: &[Target],
) -> Result<Round> {
    let here = futures::future::join_all(
        tried
            .iter()
            .map(|target| async { target.kind.control() && dial.reaches(target.address).await }),
    );
    let (pod, here) = tokio::join!(ask(cluster, name, tried), here);
    Ok(Round { pod: pod?, here })
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
async fn ask<C: Cluster>(cluster: &C, name: &str, tried: &[Target]) -> Result<Vec<bool>> {
    let agent = pod::agent_path();
    let timeout = CONNECT_SECONDS.to_string();
    let connect: Vec<String> = tried
        .iter()
        .map(|target| target.address.to_string())
        .collect();
    let mut argv = vec![agent.as_str(), "probe", "--timeout", timeout.as_str()];
    for address in &connect {
        argv.push("--connect");
        argv.push(address);
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
    read(&text, tried)
}

/// The probe's answer, whether each target answered: the last line it
/// printed, which lists targets in the order they were asked.
fn read(text: &str, tried: &[Target]) -> Result<Vec<bool>> {
    let line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .context("the probe printed nothing")?;
    let reach: Vec<Reach> =
        serde_json::from_str(line).context("the probe's answer is not readable")?;
    if reach.len() != tried.len() {
        bail!(
            "the probe answered for {} targets and was asked about {}",
            reach.len(),
            tried.len()
        );
    }
    Ok(reach.into_iter().map(|answer| answer.reachable).collect())
}

/// Each target beside its answers.
fn answers<'a>(tried: &'a [Target], round: &Round) -> Vec<Answer<'a>> {
    tried
        .iter()
        .zip(round.pod.iter().zip(&round.here))
        .map(|(target, (&reachable, &here))| Answer {
            target,
            reachable,
            here,
        })
        .collect()
}

/// What the answers say.
fn judge(answers: &[Answer<'_>]) -> ProbeOutcome {
    let open: Vec<&Target> = answers
        .iter()
        .filter(|answer| answer.reachable && answer.target.kind.closed())
        .map(|answer| answer.target)
        .collect();
    let names = |targets: &[&Target]| -> Vec<String> {
        targets.iter().map(|target| target.name.clone()).collect()
    };
    // Whatever did not answer, the policy as shipped would have shut this
    // out: the policy in force does not.
    if open.iter().any(|target| telling(target)) {
        return ProbeOutcome::Open {
            reachable: names(&open),
        };
    }
    // Shut out by the policy: a control that answered this worker, and not
    // the pod. One that answered neither says nothing.
    let shut = answers
        .iter()
        .any(|answer| answer.target.kind.control() && answer.here && !answer.reachable);
    if shut {
        if !open.is_empty() {
            return ProbeOutcome::LetThrough {
                reachable: names(&open),
            };
        }
        return match answers
            .iter()
            .find(|answer| answer.target.kind == Kind::Repository)
        {
            None => ProbeOutcome::LooksEnforced,
            Some(repo) if repo.reachable => ProbeOutcome::Enforced,
            Some(repo) => ProbeOutcome::Inconclusive {
                reason: format!(
                    "nothing answered a pod labelled like a build, the repository at {} \
                     included, which builds are allowed to reach. A pod that reaches nothing \
                     at all looks the same as an enforced policy; and builds that cannot \
                     reach the repository will not find AURCache's own packages",
                    repo.target.name
                ),
            },
        };
    }
    let silent_here: Vec<&Target> = answers
        .iter()
        .filter(|answer| answer.target.kind.control() && !answer.here)
        .map(|answer| answer.target)
        .collect();
    if open.is_empty() {
        return ProbeOutcome::Inconclusive {
            reason: format!(
                "{} did not answer this worker either while a pod labelled like a build was \
                 asked, so that the pod not reaching them says nothing of the policy",
                names(&silent_here).join(" and ")
            ),
        };
    }
    let unknown = if silent_here.is_empty() {
        String::new()
    } else {
        format!(
            "; {} did not answer this worker meanwhile, and so told nothing",
            names(&silent_here).join(" and ")
        )
    };
    ProbeOutcome::Inconclusive {
        reason: format!(
            "{} answered a pod labelled like a build, and the policy as shipped lets builds \
             reach each where what is behind it has a public address. Add those addresses \
             to the policy's `except` list: builds should not reach them, and the check can \
             then tell{unknown}",
            names(&open).join(", ")
        ),
    }
}

/// Whether a target answering a pod the policy selects means the policy is
/// not enforced: one whose address the policy as shipped shuts out. The
/// cluster's API never does, since what its Service leads to does not show.
fn telling(target: &Target) -> bool {
    match target.kind {
        Kind::Metadata => true,
        Kind::Worker => shut_out(target.address.ip()),
        Kind::Api | Kind::Repository => false,
    }
}

/// Whether `ip` is in the ranges the policy as shipped (`networkpolicy.yaml`)
/// shuts builds out of: the private, carrier-grade NAT, link-local and
/// loopback ones, and the IPv4 ones as NAT64 maps them.
fn shut_out(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [first, second, ..] = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || (first == 100 && second & 0xc0 == 64)
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            let [.., high, low] = segments;
            let nat64 = segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0];
            let embedded = Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
            v6.is_loopback()
                || segments[0] & 0xfe00 == 0xfc00
                || segments[0] & 0xffc0 == 0xfe80
                || (nat64 && shut_out(IpAddr::V4(embedded)))
        }
    }
}

/// The repository, when it was tried and did not answer.
fn silent_repo<'a>(answers: &[Answer<'a>]) -> Option<&'a str> {
    answers
        .iter()
        .find(|answer| answer.target.kind == Kind::Repository && !answer.reachable)
        .map(|answer| answer.target.name.as_str())
}

/// The probe pod, which only holds still: it carries the build pods' labels, and nothing else of theirs.
fn render(cfg: &Config, agent_image: &str) -> Pod {
    let extras = &cfg.extras;
    let mut labels = extras.labels.clone().unwrap_or_default();
    labels.extend(pod::labels(&cfg.dispatcher));
    let mut node_selector = extras.node_selector.clone().unwrap_or_default();
    node_selector.insert("kubernetes.io/os".to_string(), "linux".to_string());
    Pod {
        metadata: ObjectMeta {
            generate_name: Some(format!("aurcache-probe-{}-", cfg.dispatcher)),
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
                image_pull_policy: Some(cfg.pull_policy.clone()),
                command: Some(vec![pod::agent_path(), "hold".to_string()]),
                security_context: Some(pod::unprivileged()),
                resources: Some(pod::helper_resources()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{Admission, Exec, Sweep};
    use crate::config::{PodExtras, Tunables};
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
        async fn create(&self, pod: &Pod) -> Result<Pod, CreateError> {
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
            let mut named = pod.clone();
            let made = self.created.lock().unwrap().len();
            named.metadata.name = pod
                .metadata
                .generate_name
                .as_ref()
                .map(|prefix| format!("{prefix}{made}"));
            self.created.lock().unwrap().push(named.clone());
            let mut shown = named.clone();
            shown.status = Some(k8s_openapi::api::core::v1::PodStatus {
                phase: Some(phase.to_string()),
                ..Default::default()
            });
            self.pods.lock().unwrap().push(Arc::new(shown));
            self.generation.send_modify(|n| *n += 1);
            Ok(named)
        }
        async fn admits(&self, pod: &Pod) -> Admission {
            Admission::Admitted {
                pod: Box::new(pod.clone()),
            }
        }
        async fn delete(&self, name: &str, _: Duration) -> Result<()> {
            self.deleted.lock().unwrap().push(name.to_string());
            Ok(())
        }
        async fn sweep(&self, _: Sweep) -> Result<usize> {
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
            agent_image: "reg/agent@sha256:1".into(),
            credential: None,
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

    /// The worker port on a private address, the cluster's API when `api`,
    /// and the repository when `repo`.
    fn targets(repo: bool, api: bool) -> Targets {
        Targets {
            worker: "10.1.2.3:8083".into(),
            api: api.then(|| "10.96.0.1:443".into()),
            repo: repo.then(|| "10.1.2.3:80".into()),
        }
    }

    /// The probe's answer for the targets it was asked about, in order (the
    /// worker, the metadata address, the cluster's API when there is one,
    /// then the repository), as `reached` says.
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

    /// What answers this worker: the addresses named.
    struct Here(Vec<&'static str>);

    impl Dial for Here {
        async fn reaches(&self, address: SocketAddr) -> bool {
            self.0.contains(&address.to_string().as_str())
        }
    }

    /// Both controls of [`targets`] answer this worker.
    fn both_here() -> Here {
        Here(vec!["10.1.2.3:8083", "10.96.0.1:443", "203.0.113.7:8083"])
    }

    async fn probe_from(fake: &Fake, here: &Here, targets: &Targets) -> ProbeOutcome {
        run(
            fake,
            here,
            &config(),
            "reg/agent@sha256:1",
            targets,
            Duration::from_secs(5),
        )
        .await
    }

    async fn probe_targets(fake: &Fake, targets: &Targets) -> ProbeOutcome {
        probe_from(fake, &both_here(), targets).await
    }

    async fn probe(fake: &Fake, repo: bool, api: bool) -> ProbeOutcome {
        probe_targets(fake, &targets(repo, api)).await
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
        assert_eq!(probe(&fake, true, false).await, ProbeOutcome::Enforced);
        assert_eq!(fake.asked.lock().unwrap().len(), 1);
    }

    /// Nothing answering is what an enforced policy and a network that lets
    /// nothing through have in common.
    #[tokio::test(start_paused = true)]
    async fn a_repository_that_does_not_answer_either_is_inconclusive() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, false])]));
        let reason = inconclusive(probe(&fake, true, false).await);
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
        assert_eq!(probe(&fake, true, false).await, ProbeOutcome::Enforced);
    }

    /// With no repository to try there is no control, and closed is all
    /// that can be said.
    #[tokio::test]
    async fn closed_targets_without_a_repository_look_enforced() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false])]));
        assert_eq!(
            probe(&fake, false, false).await,
            ProbeOutcome::LooksEnforced
        );
        assert_eq!(fake.asked.lock().unwrap().len(), 1);
    }

    /// A repository whose name does not resolve is not tried, nor taken for
    /// one that did not answer.
    #[tokio::test]
    async fn a_repository_that_does_not_resolve_is_left_out() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false])]));
        let mut targets = targets(false, false);
        targets.repo = Some("aurcache-probe-test.invalid:80".into());
        assert_eq!(
            probe_targets(&fake, &targets).await,
            ProbeOutcome::LooksEnforced
        );
        let asked = fake.asked.lock().unwrap();
        assert_eq!(asked.len(), 1, "asked once");
        assert_eq!(
            asked[0].iter().filter(|word| *word == "--connect").count(),
            2
        );
    }

    /// The API answering beside the worker port is everything answering.
    #[tokio::test(start_paused = true)]
    async fn the_api_beside_the_worker_port_is_open() {
        let fake = Fake::new(Behaviour::Answers(vec![said([true, false, true, true])]));
        assert_eq!(
            probe(&fake, true, true).await,
            ProbeOutcome::Open {
                reachable: vec!["10.1.2.3:8083".into(), "10.96.0.1:443".into()]
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_api_alone_beside_a_repository_that_answers_is_let_through() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, true, true])]));
        assert_eq!(
            probe(&fake, true, true).await,
            ProbeOutcome::LetThrough {
                reachable: vec!["10.96.0.1:443".into()]
            }
        );
    }

    /// A managed control plane's public endpoint answers; the worker port's
    /// silence still shows the policy at work, though the repository is out
    /// of reach.
    #[tokio::test(start_paused = true)]
    async fn the_api_alone_beside_a_silent_repository_is_let_through() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, true, false])]));
        assert_eq!(
            probe(&fake, true, true).await,
            ProbeOutcome::LetThrough {
                reachable: vec!["10.96.0.1:443".into()]
            }
        );
    }

    /// A server outside the cluster, reached on a public address: the
    /// policy as shipped lets builds reach it, and the API's silence shows
    /// the policy at work.
    #[tokio::test(start_paused = true)]
    async fn a_public_worker_port_beside_a_closed_api_is_let_through() {
        let fake = Fake::new(Behaviour::Answers(vec![said([true, false, false, true])]));
        let mut targets = targets(true, true);
        targets.worker = "203.0.113.7:8083".into();
        assert_eq!(
            probe_targets(&fake, &targets).await,
            ProbeOutcome::LetThrough {
                reachable: vec!["203.0.113.7:8083".into()]
            }
        );
    }

    /// Both controls answering on addresses the policy as shipped may let
    /// through says nothing either way.
    #[tokio::test(start_paused = true)]
    async fn public_controls_that_answer_are_inconclusive() {
        let fake = Fake::new(Behaviour::Answers(vec![said([true, false, true, true])]));
        let mut targets = targets(true, true);
        targets.worker = "203.0.113.7:8083".into();
        let reason = inconclusive(probe_targets(&fake, &targets).await);
        assert!(
            reason.contains("203.0.113.7:8083, 10.96.0.1:443"),
            "{reason}"
        );
        assert!(reason.contains("`except`"), "{reason}");
    }

    /// A private address that answers, with nothing shut, is not enforced,
    /// whatever the repository does.
    #[tokio::test(start_paused = true)]
    async fn an_open_port_beside_a_silent_repository_is_open() {
        let fake = Fake::new(Behaviour::Answers(vec![said([true, false, false])]));
        assert_eq!(
            probe(&fake, true, false).await,
            ProbeOutcome::Open {
                reachable: vec!["10.1.2.3:8083".into()]
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_answering_worker_port_is_open() {
        let fake = Fake::new(Behaviour::Answers(vec![said([true, false, true])]));
        assert_eq!(
            probe(&fake, true, false).await,
            ProbeOutcome::Open {
                reachable: vec!["10.1.2.3:8083".into()]
            }
        );
        // The answer is checked again before it is believed.
        assert_eq!(fake.asked.lock().unwrap().len(), 1 + RECHECKS);
    }

    /// The metadata address answering beside a closed worker port: the
    /// policy as shipped shuts it out, so the policy in force is not that
    /// one, whatever it does shut.
    #[tokio::test(start_paused = true)]
    async fn an_answering_metadata_address_is_open() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, true, true])]));
        assert_eq!(
            probe(&fake, true, false).await,
            ProbeOutcome::Open {
                reachable: vec![METADATA.into()]
            }
        );
    }

    /// No policy at all, with the server down: the worker port answers
    /// nobody, and the API answers both. The worker port's silence is not
    /// the policy's.
    #[tokio::test(start_paused = true)]
    async fn no_policy_with_the_server_down_is_not_enforced() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, true, true])]));
        let reason = inconclusive(
            probe_from(&fake, &Here(vec!["10.96.0.1:443"]), &targets(true, true)).await,
        );
        assert!(reason.contains("10.96.0.1:443 answered"), "{reason}");
        assert!(
            reason.contains("10.1.2.3:8083 did not answer this worker"),
            "{reason}"
        );
        // Asked again, as anything not enforced is.
        assert_eq!(fake.asked.lock().unwrap().len(), 1 + RECHECKS);
    }

    /// Neither control answering this worker: the pod not reaching them,
    /// beside a repository that it reaches, says nothing.
    #[tokio::test(start_paused = true)]
    async fn controls_this_worker_cannot_reach_are_inconclusive() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, false, true])]));
        let reason = inconclusive(probe_from(&fake, &Here(Vec::new()), &targets(true, true)).await);
        assert!(
            reason.contains("10.1.2.3:8083 and 10.96.0.1:443 did not answer this worker"),
            "{reason}"
        );
    }

    /// One control answering this worker is enough: shut to the pod, it
    /// shows the policy at work.
    #[tokio::test]
    async fn one_control_that_answers_this_worker_is_enough() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, false, true])]));
        assert_eq!(
            probe_from(&fake, &Here(vec!["10.1.2.3:8083"]), &targets(true, true)).await,
            ProbeOutcome::Enforced
        );
    }

    /// The controls are dialled from here at each asking, at the addresses
    /// the pod is given, and nothing else is.
    #[tokio::test]
    async fn the_controls_are_dialled_at_each_asking() {
        struct Counting(Mutex<Vec<SocketAddr>>);
        impl Dial for Counting {
            async fn reaches(&self, address: SocketAddr) -> bool {
                self.0.lock().unwrap().push(address);
                true
            }
        }
        let fake = Fake::new(Behaviour::Answers(vec![
            said([true, false, false, true]),
            said([false, false, false, true]),
        ]));
        let dial = Counting(Mutex::default());
        let outcome = run(
            &fake,
            &dial,
            &config(),
            "reg/agent@sha256:1",
            &targets(true, true),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(outcome, ProbeOutcome::Enforced);
        let dialled: Vec<String> = dial
            .0
            .lock()
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            dialled,
            [
                "10.1.2.3:8083",
                "10.96.0.1:443",
                "10.1.2.3:8083",
                "10.96.0.1:443"
            ]
        );
    }

    /// A probe pod the probe no longer uses: one no node took past the
    /// wait, or any past its lifetime. One without a creation time, or
    /// younger, is in use.
    #[test]
    fn a_probe_pod_is_abandoned_past_its_wait_or_its_lifetime() {
        let now: Timestamp = "2026-11-21T12:00:00Z".parse().unwrap();
        let pod = |age: Duration, node: Option<&str>| {
            let mut pod = Pod::default();
            pod.metadata.creation_timestamp =
                Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                    now - SignedDuration::try_from(age).unwrap(),
                ));
            pod.spec = Some(PodSpec {
                node_name: node.map(ToString::to_string),
                ..PodSpec::default()
            });
            pod
        };
        let second = Duration::from_secs(1);
        assert!(!abandoned(&pod(WAIT - second, None), now));
        assert!(abandoned(&pod(WAIT, None), now));
        assert!(!abandoned(&pod(WAIT, Some("node-1")), now));
        assert!(!abandoned(&pod(LIFETIME - second, Some("node-1")), now));
        assert!(abandoned(&pod(LIFETIME, Some("node-1")), now));
        assert!(!abandoned(&Pod::default(), now));
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_that_settles_is_enforced() {
        let fake = Fake::new(Behaviour::Answers(vec![
            said([true, true, true]),
            said([false, false, true]),
        ]));
        assert_eq!(probe(&fake, true, false).await, ProbeOutcome::Enforced);
        assert_eq!(fake.asked.lock().unwrap().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn the_clusters_api_is_asked_about_too() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, true])]));
        let outcome = probe(&fake, false, true).await;
        assert_eq!(
            outcome,
            ProbeOutcome::LetThrough {
                reachable: vec!["10.96.0.1:443".into()]
            }
        );
        // It is asked again first: a CNI may still be closing in.
        assert_eq!(fake.asked.lock().unwrap().len(), 1 + RECHECKS);
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

    #[test]
    fn the_shipped_policys_ranges_are_shut_out() {
        for shut in [
            "10.0.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "169.254.169.254",
            "127.0.0.1",
            "fd00::1",
            "fe80::1",
            "::1",
            "64:ff9b::a00:1",
            "::ffff:10.0.0.1",
        ] {
            assert!(shut_out(shut.parse().unwrap()), "{shut}");
        }
        for public in [
            "203.0.113.7",
            "100.128.0.1",
            "2001:db8::1",
            "64:ff9b::cb00:7107",
        ] {
            assert!(!shut_out(public.parse().unwrap()), "{public}");
        }
    }

    #[tokio::test]
    async fn the_pod_carries_the_build_labels_and_is_deleted() {
        let fake = Fake::new(Behaviour::Answers(vec![said([false, false, true])]));
        probe(&fake, true, false).await;
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
        assert_eq!(spec.active_deadline_seconds, Some(300));
        let container = &spec.containers[0];
        assert_eq!(container.name, PROBE);
        assert_eq!(container.image.as_deref(), Some("reg/agent@sha256:1"));
        let security = container.security_context.as_ref().unwrap();
        assert_eq!(security.allow_privilege_escalation, Some(false));
        assert_eq!(security.run_as_non_root, Some(true));
        // Named by the API server, so that one an earlier run left, still
        // going away, is never in the way.
        assert_eq!(
            pod.metadata.generate_name.as_deref(),
            Some("aurcache-probe-abcdef0123456789-")
        );
        let name = pod.metadata.name.clone().unwrap();
        assert_eq!(*fake.deleted.lock().unwrap(), [name]);
    }

    #[tokio::test]
    async fn a_failed_pod_is_inconclusive_and_deleted() {
        let fake = Fake::new(Behaviour::Fails);
        inconclusive(probe(&fake, true, false).await);
        assert_eq!(fake.deleted.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pod_that_never_runs_is_inconclusive_and_deleted() {
        let fake = Fake::new(Behaviour::Hangs);
        let reason = inconclusive(probe(&fake, true, false).await);
        assert!(reason.contains("not running"), "{reason}");
        assert_eq!(fake.deleted.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_refused_pod_is_inconclusive() {
        let fake = Fake::new(Behaviour::Refused);
        let reason = inconclusive(probe(&fake, true, false).await);
        assert!(reason.contains("quota"), "{reason}");
        assert!(fake.deleted.lock().unwrap().is_empty(), "there was none");
    }

    #[tokio::test]
    async fn an_exec_that_fails_is_inconclusive_and_deleted() {
        let fake = Fake::new(Behaviour::ExecFails);
        let reason = inconclusive(probe(&fake, true, false).await);
        assert!(reason.contains("did not run"), "{reason}");
        assert_eq!(fake.deleted.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unreadable_output_is_inconclusive() {
        for text in ["", "\n", "not json\n", "[]\n"] {
            let fake = Fake::new(Behaviour::Answers(vec![text.into()]));
            inconclusive(probe(&fake, true, false).await);
            assert_eq!(fake.deleted.lock().unwrap().len(), 1);
        }
    }
}
