//! A cluster in memory, with a pod that behaves as a build pod does.
//!
//! [`FakeCluster`] implements [`Cluster`]: `create` stores the pod, the
//! status it has is whatever the test or the [`Script`] last gave it, and
//! `exec` is answered by the agent's subcommands as the script plays them,
//! over streams made of [`Step`]s. Every call is recorded. A pod moves on by
//! what the dispatcher does to it -- handing the job in starts the build,
//! reading the log to its end ends it -- so a test waits on nothing but the
//! dispatcher itself.

use anyhow::{Result, bail};
use aurcache_build_agent::report::{Artifact, BuildReport, Ended, Handback};
use aurcache_build_agent::spec::INTERFACE;
use aurcache_common::worker::{JobDescriptor, MirrorlistOffer};
use aurcache_worker_container::input::Packed;
use aurcache_worker_core::executor::ClaimedJob;
use aurcache_worker_core::protocol::{PackagesRefused, Timeout, TimeoutSource};
use aurcache_worker_kube::cluster::{Cluster, CreateError, Exec, ExecEnd, Stdin, Sweep};
use aurcache_worker_kube::config::{Config, PodExtras, Tunables};
use aurcache_worker_kube::executor::{Build, KubeExecutor};
use aurcache_worker_kube::transfer::Uploads;
use futures::FutureExt;
use k8s_openapi::api::core::v1::{Pod, PodStatus};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub const DISPATCHER: &str = "0123456789abcdef";
pub const AGENT_IMAGE: &str = "registry.example/aurcache-worker-kube@sha256:abc";

/// The name of the pod build `build_id` runs in.
pub fn pod_name(build_id: i32) -> String {
    aurcache_worker_kube::pod::name(DISPATCHER, build_id)
}

/// One thing the dispatcher asked of the cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Create(String),
    Admits(String),
    Delete {
        name: String,
        grace: Duration,
    },
    Sweep(Sweep),
    Exec {
        container: String,
        argv: Vec<String>,
    },
    LogTail {
        container: String,
        lines: i64,
    },
}

/// One piece of what an exec's stdout does.
pub enum Step {
    Bytes(Vec<u8>),
    /// Something that happens when the reader gets here.
    Then(Box<dyn FnOnce() + Send>),
    /// The stream breaks: the read fails, as a broken exec's does.
    Break,
    /// Nothing more arrives and the stream does not end.
    Hang,
    /// Nothing arrives for this long.
    Wait(Duration),
    /// A wait under way.
    Waiting(Pin<Box<tokio::time::Sleep>>),
}

/// A stream that plays its steps, a few bytes per read, then ends.
struct Played {
    steps: VecDeque<Step>,
    /// The most one read returns.
    chunk: usize,
}

impl AsyncRead for Played {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let room = buf.remaining().min(self.chunk);
            match self.steps.pop_front() {
                None => return Poll::Ready(Ok(())),
                Some(Step::Bytes(bytes)) if bytes.is_empty() => {}
                Some(Step::Bytes(mut bytes)) => {
                    let rest = bytes.split_off(room.min(bytes.len()));
                    buf.put_slice(&bytes);
                    self.steps.push_front(Step::Bytes(rest));
                    return Poll::Ready(Ok(()));
                }
                Some(Step::Then(effect)) => effect(),
                Some(Step::Break) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the exec stream broke before the process ended",
                    )));
                }
                Some(Step::Hang) => {
                    self.steps.push_front(Step::Hang);
                    return Poll::Pending;
                }
                Some(Step::Wait(pause)) => {
                    let sleep = Box::pin(tokio::time::sleep(pause));
                    self.steps.push_front(Step::Waiting(sleep));
                }
                Some(Step::Waiting(mut sleep)) => {
                    if sleep.as_mut().poll(cx).is_pending() {
                        self.steps.push_front(Step::Waiting(sleep));
                        return Poll::Pending;
                    }
                }
            }
        }
    }
}

/// An exec with no stdin whose stdout plays `steps` and which ends as `end`.
fn played(steps: Vec<Step>, chunk: usize, end: ExecEnd) -> Exec {
    Exec {
        stdin: None,
        stdout: Box::pin(Played {
            steps: steps.into(),
            chunk,
        }),
        ended: async move { end }.boxed(),
    }
}

fn lost() -> ExecEnd {
    ExecEnd::Lost {
        message: "the exec stream closed before an exit status arrived".to_string(),
    }
}

fn failed(message: &str) -> ExecEnd {
    ExecEnd::Failed {
        message: message.to_string(),
    }
}

/// A package the build left in the pod.
#[derive(Clone, Debug)]
pub struct Package {
    pub name: String,
    pub bytes: Vec<u8>,
}

impl Package {
    pub fn of(name: &str, bytes: &[u8]) -> Self {
        Self {
            name: name.to_string(),
            bytes: bytes.to_vec(),
        }
    }

    /// The line of the manifest for this package.
    pub fn artifact(&self) -> Artifact {
        Artifact {
            name: self.name.clone(),
            size: self.bytes.len() as u64,
        }
    }
}

/// What a created pod's status is before the test changes it.
#[derive(Clone, Debug)]
pub enum Start {
    /// Scheduled, with `stage` running: ready for the job.
    Staged,
    /// As the test gives it.
    Status(Box<PodStatus>),
}

/// What `receive` does with the job.
#[derive(Clone, Debug)]
pub enum Receive {
    /// Takes it; the build container starts.
    Accepts,
    Fails(String),
}

/// What becomes of the pod once the log has been read to its end.
#[derive(Clone, Debug)]
pub enum AfterLog {
    Status(Box<PodStatus>),
    /// The pod is gone from the cluster.
    Gone,
    /// The build goes on, silently, for ever.
    Nothing,
}

/// How one `send` goes.
#[derive(Clone, Debug)]
pub enum Sending {
    Whole,
    /// The stream breaks after this many bytes.
    Breaks {
        after: usize,
    },
    /// The stream ends after this many bytes, and the exec says it succeeded.
    Cut {
        after: usize,
    },
    /// The process fails without writing anything.
    Fails(String),
    /// Nothing arrives, and the stream does not end.
    Hangs,
    /// The file arrives whole, after this long.
    Slow {
        pause: Duration,
    },
}

/// How the pod of a build behaves.
pub struct Script {
    /// What `create` answers, in order; it succeeds once these are used up.
    pub creations: VecDeque<CreateError>,
    /// Why a dry run refuses the pod; `None` admits it.
    pub refusal: Option<String>,
    /// How many deletes fail before they succeed.
    pub failing_deletes: usize,
    pub start: Start,
    pub receive: Receive,
    /// The pod's status once `receive` has taken the job.
    pub after_receive: Box<PodStatus>,
    /// The build log, whole.
    pub log: Vec<u8>,
    /// For each `tail` in turn, how many bytes it gives before its stream
    /// breaks; a `tail` past these runs to the end of the log.
    pub tail_breaks: VecDeque<usize>,
    /// For each `tail` in turn once those break, how many bytes it gives
    /// before it ends in success all the same, as the API server may end a
    /// stream cut short.
    pub tail_cuts: VecDeque<usize>,
    /// The agent's report. With one, `tail` ends at the end of the log;
    /// without, it follows a log that says no more.
    pub report: Option<BuildReport>,
    pub after_log: AfterLog,
    pub packages: Vec<Package>,
    /// What `manifest` prints instead of the hand-back, when set.
    pub manifest: Option<String>,
    /// How each `send` in turn goes; whole, past these.
    pub sends: VecDeque<Sending>,
    /// The most one read of a `send` or a `tail` returns.
    pub chunk: usize,
    /// What the kubelet has of the build container's last lines.
    pub last_words: String,
    /// What `ssh-link`'s far end, the agent, does once exec'd.
    pub ssh_link: SshLinkScript,
}

/// The agent's side of the SSH link, as a test plays it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshLinkScript {
    /// Hello, these requests, the end of the stream, and then the build
    /// container ends with 0, as once the sources are downloaded the build
    /// goes on without the link.
    Asks(Vec<Vec<u8>>),
    /// The exec fails.
    Fails,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            creations: VecDeque::new(),
            refusal: None,
            failing_deletes: 0,
            start: Start::Staged,
            receive: Receive::Accepts,
            after_receive: Box::new(status::building()),
            log: b"==> Making package: hello\n==> Finished making: hello\n".to_vec(),
            tail_breaks: VecDeque::new(),
            tail_cuts: VecDeque::new(),
            report: Some(report(Ended::Exited { code: 0 })),
            after_log: AfterLog::Status(Box::new(status::built(0, "Completed"))),
            packages: vec![
                Package::of("hello-1.0-1-x86_64.pkg.tar.zst", b"the hello package"),
                Package::of(
                    "hello-docs-1.0-1-any.pkg.tar.zst",
                    b"the documentation of hello, which is longer",
                ),
            ],
            manifest: None,
            sends: VecDeque::new(),
            chunk: 7,
            last_words: String::new(),
            ssh_link: SshLinkScript::Asks(vec![vec![11]]),
        }
    }
}

impl Script {
    /// A build whose container ends with no report: `exit_code` and `reason`
    /// are what the kubelet says of it.
    pub fn unreported(exit_code: i32, reason: &str) -> Self {
        Self {
            report: None,
            after_log: AfterLog::Status(Box::new(status::built(exit_code, reason))),
            packages: Vec::new(),
            ..Self::default()
        }
    }
}

pub fn report(ended: Ended) -> BuildReport {
    BuildReport::unmeasured(ended)
}

struct Shared {
    state: Mutex<State>,
    moved: watch::Sender<u64>,
    called: watch::Sender<usize>,
}

struct State {
    pods: BTreeMap<String, Arc<Pod>>,
    /// Pods in the builds' namespace that the watch does not show: another
    /// dispatcher's, or no dispatcher's.
    strangers: BTreeMap<String, Pod>,
    calls: Vec<Call>,
    /// Every pod `create` was given, refused ones included.
    rendered: Vec<Pod>,
    /// What each `receive` read from its stdin.
    received: Vec<Vec<u8>>,
    /// What came back to the agent's side of each SSH link.
    ssh_answers: Vec<Vec<Vec<u8>>>,
    script: Script,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, call: Call) {
        let count = {
            let mut state = self.state();
            state.calls.push(call);
            state.calls.len()
        };
        self.called.send_replace(count);
    }

    fn bump(&self) {
        self.moved.send_modify(|count| *count += 1);
    }

    fn set_status(&self, name: &str, status: PodStatus) {
        {
            let mut state = self.state();
            let Some(pod) = state.pods.get(name) else {
                return;
            };
            let mut pod = Pod::clone(pod);
            pod.status = Some(status);
            state.pods.insert(name.to_string(), Arc::new(pod));
        }
        self.bump();
    }

    fn remove(&self, name: &str) {
        self.state().pods.remove(name);
        self.bump();
    }
}

/// A [`Cluster`] in memory.
pub struct FakeCluster {
    shared: Arc<Shared>,
}

impl FakeCluster {
    pub fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    pods: BTreeMap::new(),
                    strangers: BTreeMap::new(),
                    calls: Vec::new(),
                    rendered: Vec::new(),
                    received: Vec::new(),
                    ssh_answers: Vec::new(),
                    script,
                }),
                moved: watch::channel(0).0,
                called: watch::channel(0).0,
            }),
        })
    }

    pub fn calls(&self) -> Vec<Call> {
        self.shared.state().calls.clone()
    }

    /// The exec calls, each as `container: subcommand and its arguments`.
    pub fn execs(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Exec { container, argv } => {
                    Some(format!("{container}: {}", argv[1..].join(" ")))
                }
                _ => None,
            })
            .collect()
    }

    pub fn deletions(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Delete { .. }))
            .collect()
    }

    pub fn rendered(&self) -> Vec<Pod> {
        self.shared.state().rendered.clone()
    }

    pub fn received(&self) -> Vec<Vec<u8>> {
        self.shared.state().received.clone()
    }

    /// The answers each SSH link carried, a list per link.
    pub fn ssh_answers(&self) -> Vec<Vec<Vec<u8>>> {
        self.shared.state().ssh_answers.clone()
    }

    pub fn has_pod(&self, name: &str) -> bool {
        self.shared.state().pods.contains_key(name)
    }

    /// Put a pod in the cluster as if an earlier run had left it.
    pub fn leave(&self, name: &str, status: PodStatus) {
        let mut pod = Pod::default();
        pod.metadata.name = Some(name.to_string());
        pod.status = Some(status);
        self.shared
            .state()
            .pods
            .insert(name.to_string(), Arc::new(pod));
        self.shared.bump();
    }

    /// Put a pod labelled `labels` in the builds' namespace that is not this
    /// dispatcher's, so that its watch does not show it.
    pub fn leave_stranger(&self, name: &str, labels: &[(&str, &str)]) {
        let mut pod = Pod::default();
        pod.metadata.name = Some(name.to_string());
        pod.metadata.labels = Some(
            labels
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        );
        self.shared.state().strangers.insert(name.to_string(), pod);
    }

    /// Whether a pod [`Self::leave_stranger`] left is still there.
    pub fn has_stranger(&self, name: &str) -> bool {
        self.shared.state().strangers.contains_key(name)
    }

    pub fn set_status(&self, name: &str, status: PodStatus) {
        self.shared.set_status(name, status);
    }

    /// Resolves once the calls so far satisfy `met`.
    pub async fn until(&self, met: impl Fn(&[Call]) -> bool) {
        let mut called = self.shared.called.subscribe();
        loop {
            if met(&self.shared.state().calls) {
                return;
            }
            called.changed().await.unwrap();
        }
    }

    /// Resolves once `subcommand` of the agent was run in a pod.
    pub async fn until_exec(&self, subcommand: &str) {
        self.until(|calls| {
            calls.iter().any(|call| match call {
                Call::Exec { argv, .. } => argv.get(1).map(String::as_str) == Some(subcommand),
                _ => false,
            })
        })
        .await;
    }

    fn receive(&self, pod: &str) -> Exec {
        let (stdin, mut taken) = tokio::io::duplex(64 * 1024);
        let shared = Arc::clone(&self.shared);
        let pod = pod.to_string();
        let ended = async move {
            let mut job = Vec::new();
            let _ = taken.read_to_end(&mut job).await;
            let (receive, after) = {
                let mut state = shared.state();
                state.received.push(job);
                let script = &state.script;
                (script.receive.clone(), script.after_receive.clone())
            };
            match receive {
                Receive::Accepts => {
                    shared.set_status(&pod, *after);
                    ExecEnd::Succeeded
                }
                Receive::Fails(message) => failed(&message),
            }
        };
        Exec {
            stdin: Some(Box::pin(stdin)),
            stdout: Box::pin(tokio::io::empty()),
            ended: ended.boxed(),
        }
    }

    fn tail(&self, pod: &str, from: usize) -> Exec {
        let mut state = self.shared.state();
        let script = &mut state.script;
        let rest = script.log.get(from..).unwrap_or_default().to_vec();
        if let Some(after) = script.tail_breaks.pop_front() {
            let given = rest[..after.min(rest.len())].to_vec();
            return played(vec![Step::Bytes(given), Step::Break], script.chunk, lost());
        }
        if let Some(after) = script.tail_cuts.pop_front() {
            let given = rest[..after.min(rest.len())].to_vec();
            return played(vec![Step::Bytes(given)], script.chunk, ExecEnd::Succeeded);
        }
        let shared = Arc::clone(&self.shared);
        let name = pod.to_string();
        let after_log = script.after_log.clone();
        let ends = Step::Then(Box::new(move || match after_log {
            AfterLog::Status(status) => shared.set_status(&name, *status),
            AfterLog::Gone => shared.remove(&name),
            AfterLog::Nothing => {}
        }));
        let mut steps = vec![Step::Bytes(rest), ends];
        if script.report.is_none() || !matches!(script.after_log, AfterLog::Status(_)) {
            steps.push(Step::Hang);
        }
        played(steps, script.chunk, ExecEnd::Succeeded)
    }

    /// The agent's side of an SSH link: say hello, ask, record the answers,
    /// close, and end the build.
    fn ssh_link(&self, pod: &str) -> Result<Exec> {
        use aurcache_build_agent::ssh_link::HELLO;
        use tokio::io::AsyncWriteExt;
        let requests = match self.shared.state().script.ssh_link.clone() {
            SshLinkScript::Fails => bail!("container \"build\" is not running"),
            SshLinkScript::Asks(requests) => requests,
        };
        // What the agent writes, the worker reads as stdout; what the worker
        // writes to stdin, the agent reads.
        let (worker_out, mut agent_out) = tokio::io::duplex(64 * 1024);
        let (worker_in, mut agent_in) = tokio::io::duplex(64 * 1024);
        let shared = Arc::clone(&self.shared);
        let name = pod.to_string();
        let agent = tokio::spawn(async move {
            agent_out.write_all(HELLO).await.unwrap();
            let mut theirs = [0; HELLO.len()];
            agent_in.read_exact(&mut theirs).await.unwrap();
            assert_eq!(&theirs, HELLO);
            let mut answers = Vec::new();
            for message in requests {
                let len = u32::try_from(message.len()).unwrap();
                agent_out.write_all(&len.to_be_bytes()).await.unwrap();
                agent_out.write_all(&message).await.unwrap();
                let mut len = [0; 4];
                agent_in.read_exact(&mut len).await.unwrap();
                let mut answer = vec![0; u32::from_be_bytes(len) as usize];
                agent_in.read_exact(&mut answer).await.unwrap();
                answers.push(answer);
            }
            drop(agent_out);
            shared.state().ssh_answers.push(answers);
            shared.set_status(&name, status::built(0, "Completed"));
        });
        let ended = async move {
            match agent.await {
                Ok(()) => ExecEnd::Succeeded,
                Err(e) => std::panic::resume_unwind(e.into_panic()),
            }
        };
        Ok(Exec {
            stdin: Some(Box::pin(worker_in)),
            stdout: Box::pin(worker_out),
            ended: ended.boxed(),
        })
    }

    fn manifest(&self) -> Exec {
        let state = self.shared.state();
        let script = &state.script;
        let text = script.manifest.clone().unwrap_or_else(|| {
            let handback = Handback {
                interface: INTERFACE,
                report: script.report.clone(),
                log_size: script.log.len() as u64,
                artifacts: script.packages.iter().map(Package::artifact).collect(),
            };
            serde_json::to_string(&handback).unwrap() + "\n"
        });
        played(
            vec![Step::Bytes(text.into_bytes())],
            script.chunk,
            ExecEnd::Succeeded,
        )
    }

    fn send(&self, name: &str) -> Exec {
        let mut state = self.shared.state();
        let script = &mut state.script;
        let chunk = script.chunk;
        let Some(package) = script.packages.iter().find(|package| package.name == name) else {
            return played(
                Vec::new(),
                chunk,
                failed("exit code 1; stderr: no such package"),
            );
        };
        let whole = Step::Bytes(package.bytes.clone());
        match script.sends.pop_front().unwrap_or(Sending::Whole) {
            Sending::Whole => played(vec![whole], chunk, ExecEnd::Succeeded),
            Sending::Breaks { after } => {
                played(vec![truncated(whole, after), Step::Break], chunk, lost())
            }
            Sending::Cut { after } => {
                played(vec![truncated(whole, after)], chunk, ExecEnd::Succeeded)
            }
            Sending::Fails(message) => played(Vec::new(), chunk, failed(&message)),
            Sending::Hangs => played(vec![Step::Hang], chunk, lost()),
            Sending::Slow { pause } => {
                played(vec![Step::Wait(pause), whole], chunk, ExecEnd::Succeeded)
            }
        }
    }
}

/// The first `after` bytes of a package.
fn truncated(whole: Step, after: usize) -> Step {
    let Step::Bytes(mut bytes) = whole else {
        panic!("only a package of bytes can end part way");
    };
    bytes.truncate(after);
    Step::Bytes(bytes)
}

impl Cluster for FakeCluster {
    async fn create(&self, pod: &Pod) -> Result<(), CreateError> {
        let name = pod.metadata.name.clone().unwrap();
        self.shared.record(Call::Create(name.clone()));
        {
            let mut state = self.shared.state();
            state.rendered.push(pod.clone());
            if let Some(refusal) = state.script.creations.pop_front() {
                return Err(refusal);
            }
            let mut stored = pod.clone();
            stored.status = Some(match &state.script.start {
                Start::Staged => status::staged(),
                Start::Status(status) => (**status).clone(),
            });
            state.pods.insert(name, Arc::new(stored));
        }
        self.shared.bump();
        Ok(())
    }

    async fn admits(&self, pod: &Pod) -> Result<()> {
        self.shared
            .record(Call::Admits(pod.metadata.name.clone().unwrap()));
        match &self.shared.state().script.refusal {
            Some(refusal) => bail!("{refusal}"),
            None => Ok(()),
        }
    }

    async fn delete(&self, name: &str, grace: Duration) -> Result<()> {
        self.shared.record(Call::Delete {
            name: name.to_string(),
            grace,
        });
        {
            let mut state = self.shared.state();
            if state.script.failing_deletes > 0 {
                state.script.failing_deletes -= 1;
                bail!("deleting pod {name}: the API server is not there");
            }
        }
        self.shared.remove(name);
        Ok(())
    }

    async fn sweep(&self, which: Sweep) -> Result<usize> {
        self.shared.record(Call::Sweep(which));
        let swept = {
            let mut state = self.shared.state();
            let managed = |pod: &Pod| {
                pod.metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("app.kubernetes.io/managed-by"))
                    .is_some_and(|by| by == aurcache_worker_kube::pod::MANAGED_BY)
            };
            let strangers = match which {
                Sweep::Own => 0,
                Sweep::Namespace => {
                    let before = state.strangers.len();
                    state.strangers.retain(|_, pod| !managed(pod));
                    before - state.strangers.len()
                }
            };
            std::mem::take(&mut state.pods).len() + strangers
        };
        self.shared.bump();
        Ok(swept)
    }

    fn pod(&self, name: &str) -> Option<Arc<Pod>> {
        self.shared.state().pods.get(name).cloned()
    }

    fn pods(&self) -> Vec<Arc<Pod>> {
        self.shared.state().pods.values().cloned().collect()
    }

    fn generation(&self) -> watch::Receiver<u64> {
        self.shared.moved.subscribe()
    }

    async fn exec(&self, pod: &str, container: &str, argv: &[&str], stdin: Stdin) -> Result<Exec> {
        self.shared.record(Call::Exec {
            container: container.to_string(),
            argv: argv.iter().map(ToString::to_string).collect(),
        });
        if !self.has_pod(pod) {
            bail!("pods {pod:?} not found");
        }
        let value = |flag: &str| {
            let at = argv.iter().position(|word| *word == flag).unwrap();
            argv[at + 1]
        };
        Ok(match argv[1] {
            "receive" => {
                assert_eq!(stdin, Stdin::Piped, "receive reads the job from stdin");
                self.receive(pod)
            }
            "tail" => self.tail(pod, value("--from").parse().unwrap()),
            "manifest" => self.manifest(),
            "send" => self.send(
                argv.iter()
                    .find_map(|word| word.strip_prefix("--name="))
                    .expect("send takes its name as one `--name=` word"),
            ),
            "ssh-link" => {
                assert_eq!(container, aurcache_worker_kube::pod::BUILD);
                assert_eq!(stdin, Stdin::Piped, "ssh-link carries the link on stdin");
                self.ssh_link(pod)?
            }
            other => panic!("the agent has no subcommand {other:?} for a build"),
        })
    }

    async fn log_tail(&self, _pod: &str, container: &str, lines: i64) -> Result<String> {
        self.shared.record(Call::LogTail {
            container: container.to_string(),
            lines,
        });
        Ok(self.shared.state().script.last_words.clone())
    }
}

/// Pod statuses, as the kubelet writes them at each point of a build.
pub mod status {
    use aurcache_worker_kube::pod::{BUILD, HOLD, STAGE};
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
        ContainerStatus, PodCondition, PodStatus,
    };

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

    fn scheduled(stage: ContainerState, rest: Vec<ContainerStatus>) -> PodStatus {
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
}

/// One upload as the sink saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Taken {
    pub build_id: i32,
    pub name: String,
    /// The length declared for it.
    pub len: u64,
    /// The bytes read from the body: as many as were declared, when it had
    /// them.
    pub bytes: Vec<u8>,
}

/// What the sink answers an upload with.
#[derive(Clone, Debug)]
pub enum Answer {
    /// The server will not take this file, whoever sends it.
    Refuses(String),
}

/// An [`Uploads`] that keeps what it is sent.
///
/// It reads the bytes an upload declares and no further, which is how an
/// HTTP/1.1 request with a `Content-Length` reads its body.
#[derive(Default)]
pub struct Sink {
    taken: Mutex<Vec<Taken>>,
    answers: Mutex<VecDeque<Answer>>,
}

impl Sink {
    pub fn answering(answers: impl IntoIterator<Item = Answer>) -> Self {
        Self {
            taken: Mutex::default(),
            answers: Mutex::new(answers.into_iter().collect()),
        }
    }

    pub fn taken(&self) -> Vec<Taken> {
        self.taken.lock().unwrap().clone()
    }
}

impl Uploads for Sink {
    async fn upload<R>(&self, build_id: i32, name: &str, body: R, len: u64) -> Result<()>
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let answer = self.answers.lock().unwrap().pop_front();
        if let Some(Answer::Refuses(reason)) = answer {
            return Err(PackagesRefused(format!("artifact refused: {reason}")).into());
        }
        let mut bytes = Vec::new();
        let read = body.take(len).read_to_end(&mut bytes).await;
        let whole = bytes.len() as u64 == len;
        self.taken.lock().unwrap().push(Taken {
            build_id,
            name: name.to_string(),
            len,
            bytes,
        });
        read?;
        if !whole {
            bail!("the request body ended before its declared length");
        }
        Ok(())
    }
}

pub fn config() -> Config {
    Config {
        namespace: "aurcache-builds".to_string(),
        dispatcher: DISPATCHER.to_string(),
        builder_image: "registry.example/builder:latest".to_string(),
        pull_policy: "Always".to_string(),
        agent_image: AGENT_IMAGE.to_string(),
        credential: None,
        extras: PodExtras::default(),
        tunables: Tunables {
            memory: Some(4 << 30),
            cpus: None,
            disk: Some(30 << 30),
            schedule_timeout: Duration::from_secs(600),
            keyserver: "hkps://keys.example".to_string(),
        },
    }
}

pub fn job(build_id: i32) -> JobDescriptor {
    JobDescriptor {
        build_id,
        pkgbase: "hello".to_string(),
        arch: "x86_64".to_string(),
        build_flags: Vec::new(),
        persistent_builddir: false,
        makepkg_conf: String::new(),
        pacman_conf: String::new(),
        mirrorlist: MirrorlistOffer::Nothing,
        pgp_keys: Vec::new(),
        vcs_sources: Vec::new(),
        expected_packages: vec!["hello".to_string()],
        timeout_secs: None,
        expected_version: "1.0-1".to_string(),
        ssh_key: None,
    }
}

/// A job as the runner hands it over, with the server's timeout when given.
pub fn claimed(build_id: i32, timeout: Option<Duration>) -> ClaimedJob {
    ClaimedJob {
        job: job(build_id),
        mirrorlist: None,
        timeout: timeout.map(|limit| Timeout {
            limit,
            set_by: TimeoutSource::Server,
        }),
        cancel: CancellationToken::new(),
    }
}

/// What a build came to: its report, and everything written to its log.
pub struct Built {
    pub report: aurcache_common::worker::CompleteReport,
    pub log: String,
}

pub type KubeExecutorOverFake = KubeExecutor<FakeCluster>;

/// A dispatcher over a fake cluster and a recording sink.
pub struct Rig {
    pub cluster: Arc<FakeCluster>,
    pub executor: KubeExecutor<FakeCluster>,
    pub sink: Sink,
    /// The socket of the ssh-agent builds are led to, when a test sets one.
    pub ssh: Option<std::path::PathBuf>,
}

/// The job as `receive` reads it in a test: any bytes, since no agent reads
/// them.
pub const INPUT: &[u8] = b"the job, and its source";

/// [`INPUT`] as the dispatcher holds it, in two pieces that are written one
/// after the other.
#[must_use]
pub fn input() -> Packed {
    Packed {
        head: b"the job, ".to_vec(),
        source: b"and its source".to_vec(),
    }
}

impl Rig {
    pub fn new(script: Script) -> Self {
        Self::with(script, Sink::default())
    }

    pub fn with(script: Script, sink: Sink) -> Self {
        Self::configured(script, sink, config())
    }

    /// A rig whose dispatcher has `cfg`.
    pub fn configured(script: Script, sink: Sink, cfg: Config) -> Self {
        let cluster = FakeCluster::new(script);
        Self {
            executor: KubeExecutor::new(cfg, Arc::clone(&cluster)),
            cluster,
            sink,
            ssh: None,
        }
    }

    /// Build `claimed` to its report, collecting its log.
    pub async fn build(&self, claimed: &ClaimedJob) -> Built {
        let (log, mut lines) = tokio::sync::mpsc::channel(16);
        let build = Build {
            claimed,
            input: input(),
            log,
            uploads: &self.sink,
            ssh: self.ssh.clone(),
        };
        let collect = async {
            let mut text = String::new();
            while let Some(chunk) = lines.recv().await {
                text.push_str(&chunk);
            }
            text
        };
        let (report, log) = tokio::join!(self.executor.build(build), collect);
        Built {
            report: report.expect("the build should end in a report"),
            log,
        }
    }
}
