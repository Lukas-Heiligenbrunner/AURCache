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
use aurcache_worker_kube::pod::{
    BUILD, HOLD, LABEL_DISPATCHER, LABEL_MANAGED_BY, MANAGED_BY, STAGE,
};
use aurcache_worker_kube::transfer::Uploads;
use bytes::Bytes;
use futures::FutureExt;
use k8s_openapi::api::core::v1::{Pod, PodStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::watch;
use tokio_util::io::StreamReader;
use tokio_util::sync::CancellationToken;

pub const DISPATCHER: &str = "0123456789abcdef";
pub const AGENT_IMAGE: &str = "registry.example/aurcache-worker-kube@sha256:abc";

/// The name of the pod that build `build_id` runs in.
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
    /// What a `receive` read from its stdin.
    Received(Vec<u8>),
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
}

/// An exec with no stdin whose stdout plays `steps`, at most `chunk` bytes
/// per read, and which ends as `end`.
fn played(steps: Vec<Step>, chunk: usize, end: ExecEnd) -> Exec {
    let stream = futures::stream::unfold(VecDeque::from(steps), move |mut steps| async move {
        loop {
            match steps.pop_front()? {
                Step::Bytes(bytes) if bytes.is_empty() => {}
                Step::Bytes(mut bytes) => {
                    let rest = bytes.split_off(chunk.min(bytes.len()));
                    steps.push_front(Step::Bytes(rest));
                    return Some((Ok(Bytes::from(bytes)), steps));
                }
                Step::Then(effect) => effect(),
                Step::Break => {
                    let broke = io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the exec stream broke before the process ended",
                    );
                    return Some((Err(broke), steps));
                }
                Step::Hang => std::future::pending::<()>().await,
                Step::Wait(pause) => tokio::time::sleep(pause).await,
            }
        }
    });
    Exec {
        stdin: None,
        stdout: Box::pin(StreamReader::new(stream)),
        ended: async move { end }.boxed(),
    }
}

fn lost() -> ExecEnd {
    failed("the exec stream closed before an exit status arrived")
}

fn failed(message: &str) -> ExecEnd {
    ExecEnd::Failed {
        message: message.to_string(),
    }
}

/// A package that the build left in the pod.
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

/// What `receive` does with the job.
#[derive(Clone, Debug)]
pub enum Receive {
    /// Takes it; the build container starts.
    Accepts,
    Fails(String),
    /// Reads it, and never exits.
    Hangs,
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

/// How one `send` goes, or one `tail` that does not follow the log to its
/// end.
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
    /// Why the first `create` is refused; `None` creates the pod.
    pub creation: Option<CreateError>,
    /// Why a dry run refuses the pod; `None` admits it.
    pub refusal: Option<String>,
    /// How many deletes fail before they succeed.
    pub failing_deletes: usize,
    /// The status of a created pod, before anything changes it.
    pub start: PodStatus,
    pub receive: Receive,
    /// The pod's status once `receive` has taken the job.
    pub after_receive: PodStatus,
    /// The build log, whole.
    pub log: Vec<u8>,
    /// How each `tail` in turn goes, from its offset; past these, a `tail`
    /// follows the log to its end.
    pub tails: VecDeque<Sending>,
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
}

impl Default for Script {
    fn default() -> Self {
        Self {
            creation: None,
            refusal: None,
            failing_deletes: 0,
            start: status::staged(),
            receive: Receive::Accepts,
            after_receive: status::building(),
            log: b"==> Making package: hello\n==> Finished making: hello\n".to_vec(),
            tails: VecDeque::new(),
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
    /// Every pod in the builds' namespace; the watch shows this dispatcher's.
    pods: BTreeMap<String, Arc<Pod>>,
    calls: Vec<Call>,
    /// Every pod that `create` was given, refused ones included.
    rendered: Vec<Pod>,
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
                    calls: Vec::new(),
                    rendered: Vec::new(),
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

    /// The exec calls of `subcommand`, each as for [`Self::execs`].
    pub fn execs_of(&self, subcommand: &str) -> Vec<String> {
        let prefix = format!(": {subcommand}");
        let execs = self.execs().into_iter();
        execs.filter(|exec| exec.contains(&prefix)).collect()
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

    pub fn has_pod(&self, name: &str) -> bool {
        self.shared.state().pods.contains_key(name)
    }

    /// Put a pod labelled `labels` in the builds' namespace as if an earlier
    /// run had left it, created `age` ago.
    pub fn leave(
        &self,
        name: &str,
        labels: BTreeMap<String, String>,
        status: PodStatus,
        age: Duration,
    ) {
        let mut pod = Pod::default();
        pod.metadata.name = Some(name.to_string());
        pod.metadata.labels = Some(labels);
        let age = SignedDuration::try_from(age).unwrap();
        pod.metadata.creation_timestamp = Some(Time(Timestamp::now() - age));
        pod.status = Some(status);
        self.shared
            .state()
            .pods
            .insert(name.to_string(), Arc::new(pod));
        self.shared.bump();
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

    fn receive(&self, pod: &str) -> Exec {
        let (stdin, mut taken) = tokio::io::duplex(64 * 1024);
        let shared = Arc::clone(&self.shared);
        let pod = pod.to_string();
        let ended = async move {
            let mut job = Vec::new();
            let _ = taken.read_to_end(&mut job).await;
            shared.record(Call::Received(job));
            let receive = shared.state().script.receive.clone();
            match receive {
                Receive::Accepts => {
                    let after = shared.state().script.after_receive.clone();
                    shared.set_status(&pod, after);
                    ExecEnd::Succeeded
                }
                Receive::Fails(message) => failed(&message),
                Receive::Hangs => std::future::pending().await,
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
        if let Some(how) = script.tails.pop_front() {
            return playing(how, rest, script.chunk);
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

    fn manifest(&self) -> Exec {
        let state = self.shared.state();
        let script = &state.script;
        let text = script.manifest.clone().unwrap_or_else(|| {
            let handback = Handback {
                interface: INTERFACE,
                report: script.report.clone(),
                log_size: Some(script.log.len() as u64),
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
        let how = script.sends.pop_front().unwrap_or(Sending::Whole);
        playing(how, package.bytes.clone(), chunk)
    }
}

/// An `ssh-link` that ends at once, as an agent that took no link would.
fn ssh_link() -> Exec {
    Exec {
        stdin: Some(Box::pin(tokio::io::sink())),
        stdout: Box::pin(tokio::io::empty()),
        ended: async { ExecEnd::Succeeded }.boxed(),
    }
}

/// An exec whose stdout gives `bytes` as `how` says.
fn playing(how: Sending, mut bytes: Vec<u8>, chunk: usize) -> Exec {
    match how {
        Sending::Whole => played(vec![Step::Bytes(bytes)], chunk, ExecEnd::Succeeded),
        Sending::Breaks { after } => {
            bytes.truncate(after);
            played(vec![Step::Bytes(bytes), Step::Break], chunk, lost())
        }
        Sending::Cut { after } => {
            bytes.truncate(after);
            played(vec![Step::Bytes(bytes)], chunk, ExecEnd::Succeeded)
        }
        Sending::Fails(message) => played(Vec::new(), chunk, failed(&message)),
        Sending::Hangs => played(vec![Step::Hang], chunk, lost()),
        Sending::Slow { pause } => played(
            vec![Step::Wait(pause), Step::Bytes(bytes)],
            chunk,
            ExecEnd::Succeeded,
        ),
    }
}

fn label<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
    pod.metadata.labels.as_ref()?.get(key).map(String::as_str)
}

/// Whether the watch of this dispatcher shows `pod`.
fn own(pod: &Pod) -> bool {
    label(pod, LABEL_DISPATCHER) == Some(DISPATCHER)
}

impl Cluster for FakeCluster {
    async fn create(&self, pod: &Pod) -> Result<(), CreateError> {
        let name = pod.metadata.name.clone().unwrap();
        self.shared.record(Call::Create(name.clone()));
        {
            let mut state = self.shared.state();
            state.rendered.push(pod.clone());
            if let Some(refusal) = state.script.creation.take() {
                return Err(refusal);
            }
            let mut stored = pod.clone();
            stored.metadata.creation_timestamp = Some(Time(Timestamp::now()));
            stored.status = Some(state.script.start.clone());
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
            let pods = &mut self.shared.state().pods;
            let before = pods.len();
            pods.retain(|_, pod| match which {
                Sweep::Own => !own(pod),
                Sweep::Namespace => label(pod, LABEL_MANAGED_BY) != Some(MANAGED_BY),
            });
            before - pods.len()
        };
        self.shared.bump();
        Ok(swept)
    }

    fn pod(&self, name: &str) -> Option<Arc<Pod>> {
        self.shared
            .state()
            .pods
            .get(name)
            .filter(|pod| own(pod))
            .cloned()
    }

    fn pods(&self) -> Vec<Arc<Pod>> {
        let state = self.shared.state();
        state
            .pods
            .values()
            .filter(|pod| own(pod))
            .cloned()
            .collect()
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
                assert_eq!(container, BUILD);
                assert_eq!(stdin, Stdin::Piped, "ssh-link carries the link on stdin");
                ssh_link()
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
pub mod status;

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

/// An [`Uploads`] that keeps what it is sent.
///
/// It reads the bytes that an upload declares and no further, which is how an
/// HTTP/1.1 request with a `Content-Length` reads its body.
#[derive(Default)]
pub struct Sink {
    taken: Mutex<Vec<Taken>>,
    /// Why the server refuses the first upload, whoever sends it; it takes
    /// the others.
    refusal: Mutex<Option<String>>,
}

impl Sink {
    pub fn refusing(refusal: String) -> Self {
        Self {
            taken: Mutex::default(),
            refusal: Mutex::new(Some(refusal)),
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
        if let Some(reason) = self.refusal.lock().unwrap().take() {
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
            memory_bytes: Some(4 << 30),
            cpus: None,
            disk_bytes: Some(30 << 30),
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

impl Built {
    pub fn reason(&self) -> &str {
        self.report.reason.as_deref().unwrap_or_default()
    }

    /// The build's own lines of the log, without the worker's.
    pub fn forwarded_log(&self) -> String {
        let lines = self
            .log
            .lines()
            .filter(|line| !line.starts_with("[worker]"));
        lines.collect::<Vec<_>>().join("\n")
    }
}

/// A dispatcher over a fake cluster and a recording sink.
pub struct Rig {
    pub cluster: Arc<FakeCluster>,
    pub executor: KubeExecutor<FakeCluster>,
    pub sink: Sink,
    /// The socket of the ssh-agent that builds are led to, when a test sets
    /// one.
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
        let cluster = FakeCluster::new(script);
        Self {
            executor: KubeExecutor::new(config(), Arc::clone(&cluster)),
            cluster,
            sink: Sink::default(),
            ssh: None,
        }
    }

    /// Build `claimed` to its report, collecting its log. The timeout catches
    /// a build that stalls idle, not one that spins.
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
        let ended =
            tokio::time::timeout(Duration::from_secs(2 * 86_400), self.executor.build(build));
        let (report, log) = tokio::join!(ended, collect);
        Built {
            report: report
                .expect("the build never ended")
                .expect("the build should end in a report"),
            log,
        }
    }
}
