//! The lifecycle of one build: a claimed job becomes a pod, a build log,
//! streamed packages and one report.
//!
//! `kubernetes-executor.md` describes it, from "Shape" to "How a build ended".
//! In order: the pod is created; the job is handed in once its `stage`
//! container runs; the log is followed while the build container runs; when
//! that container has ended, what it left is read and each package is
//! relayed to the server. Whatever happens on the way, the pod is deleted
//! before the report is returned.

use crate::cluster::{Admission, Cluster, CreateError};
use crate::config::Config;
use crate::outcome::{self, BuildEnd, Observed};
use crate::pod::{self, BUILD, PodInputs};
use crate::transfer::{Agent, Lines, Uploads};
use anyhow::{Context, Result, anyhow, bail};
use aurcache_build_agent::report::Artifact;
use aurcache_build_agent::spec::Credential;
use aurcache_common::artifact::is_artifact;
use aurcache_common::worker::{BuildOutcome, CompleteReport, JobDescriptor, MirrorlistOffer};
use aurcache_worker_container::outcome::{Ending, MemoryLimit, complete};
use aurcache_worker_container::spec::SpecOptions;
use aurcache_worker_container::{input, spec, unsupported};
use aurcache_worker_core::build_log::BuildLog;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::executor::{ClaimedJob, Executor};
use aurcache_worker_core::live::Live;
use aurcache_worker_core::protocol::{PackagesRefused, Stop, StopWatch};
use aurcache_worker_core::report;
use aurcache_worker_core::settings::WorkerSettings;
use futures::FutureExt;
use k8s_openapi::api::core::v1::Pod;
use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, resume_unwind};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// How long a pod's containers are given to end when it is deleted.
const GRACE: Duration = Duration::from_secs(5);
/// How long the log may take to reach its end once the build container has
/// ended with a report, which is when `tail` ends by itself.
const LOG_END: Duration = Duration::from_secs(60);
/// How long the log is read on once the build container has ended without a
/// report, when nothing says where its end is.
const LOG_DRAIN: Duration = Duration::from_secs(3);
/// How long a leftover pod with the name a build needs may take to go away.
const REPLACE: Duration = Duration::from_secs(60);
/// How long an image that cannot be pulled is waited for. The kubelet tries
/// again by itself, at growing intervals, and a registry that answers within
/// this is not worth one of the build's attempts.
const PULL_PATIENCE: Duration = Duration::from_secs(2 * 60);
/// How many of the build container's last lines are copied into the build
/// log when it ended without a report.
const LAST_LINES: i64 = 20;
/// The variable an operator raises the per-build memory limit with.
const MEMORY_VARIABLE: &str = "WORKER_BUILD_MEMORY_MAX";
/// Where the build's own trees go, under the work volume, so that everything
/// a build writes is in the volume its disk limit is on.
const SCRATCH: &str = "scratch";
/// The key file of a `kubernetes.io/ssh-auth` Secret, and the host keys
/// that go beside it.
const CREDENTIAL_KEY: &str = "ssh-privatekey";
const CREDENTIAL_KNOWN_HOSTS: &str = "known_hosts";
/// The build the admission check asks about: an id no build has.
const PROBE_BUILD: i32 = 0;
/// The timeout the admission check's pod carries, so that it has a deadline
/// like the pods of the builds it stands for and a quota scoped to pods with
/// one counts it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Builds each job in a pod of its own.
pub struct KubeExecutor<C: Cluster> {
    /// The configuration in force. Each build takes the one current when it
    /// starts and keeps it, so a pod is never rendered under one limit and
    /// reported under another.
    cfg: Live<Config>,
    cluster: Arc<C>,
    agent_image: String,
    /// What the last readiness check found, so that a change is logged once.
    readiness: Mutex<Readiness>,
}

/// What one build is made from, beside the configuration.
pub struct Build<'a, U> {
    pub claimed: &'a ClaimedJob,
    /// The job as `receive` reads it: [`input::pack`]'s tar.
    pub input: Vec<u8>,
    /// Where the build log goes, a chunk of text at a time.
    pub log: mpsc::Sender<String>,
    pub uploads: &'a U,
}

/// Whether the cluster can start one more build now.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Readiness {
    Ready,
    /// The cluster did not answer whether it admits a build pod.
    Unsure {
        message: String,
    },
    /// One of this dispatcher's pods has no node to go to.
    Unschedulable {
        pod: String,
        message: String,
    },
    /// The cluster would not admit a build pod.
    Refused {
        message: String,
    },
}

/// How far a build got before something other than its end decided it.
enum Reached {
    /// The build container ended; what it left is in the pod.
    Built(BuildEnd),
    /// The build is over with this report, and nothing to hand back.
    Over(CompleteReport),
}

/// Whether a pod of the name a build needs could be created.
enum Created {
    Yes,
    Refused {
        message: String,
    },
    /// The build was stopped while a leftover pod of that name went away.
    Stopped(Stop),
}

/// What a pod is waited for before it can be handed its job.
enum Awaited {
    /// A node, with the scheduler's message when it has said why there is
    /// none.
    Node(Option<String>),
    /// Its `stage` container, with the pod's own word for where it is.
    Stage(String),
}

impl<C: Cluster> KubeExecutor<C> {
    pub fn new(cfg: Config, cluster: Arc<C>, agent_image: String) -> Self {
        Self {
            cfg: Live::new(cfg),
            cluster,
            agent_image,
            readiness: Mutex::new(Readiness::Ready),
        }
    }

    /// Delete the pods a previous process of this dispatcher left: the server
    /// gives up every build that process held as soon as this one says it
    /// holds none, so nothing will ever collect them otherwise. How many
    /// there were.
    pub async fn sweep(&self) -> Result<usize> {
        let swept = self
            .cluster
            .sweep()
            .await
            .context("deleting the build pods a previous run left")?;
        if swept > 0 {
            tracing::warn!(
                "Deleted {swept} build pod(s) left by a previous run of this worker; the \
                 server ends their builds, and queues a new build of each package while it \
                 has attempts left"
            );
        }
        Ok(swept)
    }

    /// Build one job whose input is packed: everything [`Executor::run_job`]
    /// does once it has the job's source.
    pub async fn build<U: Uploads>(&self, build: Build<'_, U>) -> Result<CompleteReport> {
        let cfg = self.cfg.current();
        let pod = self.render(&cfg, build.claimed)?;
        self.run(&cfg, &pod, &build).await
    }

    fn render(&self, cfg: &Config, claimed: &ClaimedJob) -> Result<Pod> {
        pod::render(&PodInputs {
            cfg,
            agent_image: &self.agent_image,
            job: &claimed.job,
            timeout: claimed.timeout.map(|timeout| timeout.limit),
        })
    }

    /// Create the pod, see the build through in it, and delete it: on every
    /// way out, a panic of this task's own making included.
    async fn run<U: Uploads>(
        &self,
        cfg: &Config,
        pod: &Pod,
        build: &Build<'_, U>,
    ) -> Result<CompleteReport> {
        let watch = build.claimed.watch();
        let name = pod::name(&cfg.dispatcher, build.claimed.job.build_id);
        let attended = AssertUnwindSafe(self.attend(cfg, pod, &name, build, &watch))
            .catch_unwind()
            .await;
        if let Err(error) = self.cluster.delete(&name, GRACE).await {
            // Its deadline ends it, and the next start of this worker sweeps
            // it; neither is a reason to change what the build is reported as.
            tracing::warn!("{error:#}");
        }
        attended.unwrap_or_else(|panic| resume_unwind(panic))
    }

    /// The build from pod creation to its report, stopped where the job's
    /// cancellation or timeout says.
    ///
    /// A timeout bounds the build itself: once the build container has ended
    /// it no longer applies, and only a cancellation stops the hand-back.
    async fn attend<U: Uploads>(
        &self,
        cfg: &Config,
        pod: &Pod,
        name: &str,
        build: &Build<'_, U>,
        watch: &StopWatch,
    ) -> Result<CompleteReport> {
        let bin = pod::agent_path();
        let agent = Agent {
            cluster: &*self.cluster,
            pod: name,
            bin: &bin,
        };
        let lines = Mutex::new(Lines::default());
        let reached = tokio::select! {
            biased;
            reached = self.until_built(cfg, pod, &agent, build, &lines, watch) => Ok(reached),
            stop = watch.stopped() => Err(stop),
        };
        let rest = lock(&lines).flush();
        if let Some(mut rest) = rest {
            // What the dispatcher says next starts a line of its own.
            if !rest.ends_with('\n') {
                rest.push('\n');
            }
            say(&build.log, &rest).await;
        }
        let end = match reached {
            Err(stop) => return Ok(stopped(stop, watch)),
            Ok(reached) => match reached? {
                Reached::Over(report) => return Ok(report),
                Reached::Built(end) => end,
            },
        };
        tokio::select! {
            biased;
            report = self.hand_back(cfg, &agent, build, &end, watch) => Ok(report),
            () = build.claimed.cancel.cancelled() => Ok(stopped(Stop::Canceled, watch)),
        }
    }

    /// Create the pod, hand the job in, and follow the build until its
    /// container has ended or the pod is lost.
    async fn until_built<U: Uploads>(
        &self,
        cfg: &Config,
        pod: &Pod,
        agent: &Agent<'_, C>,
        build: &Build<'_, U>,
        lines: &Mutex<Lines>,
        watch: &StopWatch,
    ) -> Result<Reached> {
        let log = &build.log;
        if let Created::Refused { message } = self.create(pod, agent.pod).await? {
            return Ok(Reached::Over(outcome::refused(&message)));
        }
        say(log, &format!("[worker] created pod {}\n", agent.pod)).await;
        let mut status = Status::new(agent.pod, self.cluster.generation());
        if let Some(over) = self.until_staged(cfg, build, &mut status, watch).await? {
            return Ok(Reached::Over(over));
        }
        say(log, "[worker] handing the job to the pod\n").await;
        // Once, and never again: a second hand-in is refused by the agent,
        // and after a failed one nobody knows whether a build started.
        if let Err(error) = agent.receive(&build.input).await {
            return Ok(Reached::Over(report::interrupted(format!(
                "handing the job to the build pod failed: {error:#}"
            ))));
        }

        let follow = agent.follow(lines, log);
        tokio::pin!(follow);
        let mut followed = false;
        let end = loop {
            match status.read(&*self.cluster, log).await {
                Seen::Gone => return Ok(Reached::Over(outcome::gone())),
                Seen::Pod(Observed::Built(end)) => break end,
                Seen::Pod(Observed::Lost(loss)) => {
                    return Ok(Reached::Over(lost(cfg, build, &loss, watch)));
                }
                Seen::NotYet | Seen::Pod(_) => {}
            }
            tokio::select! {
                changed = status.changed() => changed?,
                () = &mut follow, if !followed => followed = true,
            }
        };
        if !followed {
            // A report exists exactly when the agent exited cleanly, and
            // `tail` then ends by itself at the end of the log.
            let wait = if end.exit_code == 0 {
                LOG_END
            } else {
                LOG_DRAIN
            };
            if tokio::time::timeout(wait, &mut follow).await.is_err() && end.exit_code == 0 {
                tracing::warn!(
                    "build {}: the log of {} did not reach its end; reporting what arrived",
                    build.claimed.job.build_id,
                    agent.pod
                );
            }
        }
        Ok(Reached::Built(end))
    }

    /// Create the pod. One of that name left by an earlier attempt at the
    /// same build is deleted first.
    async fn create(&self, pod: &Pod, name: &str) -> Result<Created> {
        match self.cluster.create(pod).await {
            Ok(()) => return Ok(Created::Yes),
            Err(CreateError::Refused { message }) => return Ok(Created::Refused { message }),
            Err(CreateError::Other(error)) => return Err(error),
            Err(CreateError::Exists) => {}
        }
        tracing::warn!("pod {name} exists already; deleting it to build in a fresh one");
        self.cluster.delete(name, Duration::ZERO).await?;
        let mut generation = self.cluster.generation();
        let gone = async {
            while self.cluster.pod(name).is_some() {
                if generation.changed().await.is_err() {
                    return;
                }
            }
        };
        if tokio::time::timeout(REPLACE, gone).await.is_err() {
            bail!("a pod named {name} exists already and did not go away when deleted");
        }
        match self.cluster.create(pod).await {
            Ok(()) => Ok(Created::Yes),
            Err(CreateError::Refused { message }) => Ok(Created::Refused { message }),
            Err(CreateError::Exists) => bail!("a pod named {name} exists already"),
            Err(CreateError::Other(error)) => Err(error),
        }
    }

    /// Wait for the pod's `stage` container to run, which is when the job
    /// can be handed in. `Some` is the report of a build that ended here.
    async fn until_staged<U: Uploads>(
        &self,
        cfg: &Config,
        build: &Build<'_, U>,
        status: &mut Status,
        watch: &StopWatch,
    ) -> Result<Option<CompleteReport>> {
        // The same allowance twice: for a node to take the pod, and then
        // for the pod to start on it. Neither is the build's own time, and a
        // pod that cannot start (a volume that will not mount, a network the
        // node cannot give it) says so in events, not in its status.
        let timeout = cfg.tunables.schedule_timeout;
        let scheduled_by = Instant::now() + timeout;
        let mut started_by = None;
        loop {
            let awaited = match status.read(&*self.cluster, &build.log).await {
                Seen::Gone => return Ok(Some(outcome::gone())),
                Seen::NotYet => Awaited::Node(None),
                Seen::Pod(Observed::Unscheduled { unschedulable }) => Awaited::Node(unschedulable),
                Seen::Pod(starting @ (Observed::Starting { .. } | Observed::Unpulled { .. })) => {
                    Awaited::Stage(starting.line().unwrap_or_default())
                }
                Seen::Pod(Observed::Staged) => return Ok(None),
                Seen::Pod(Observed::Lost(loss)) => {
                    return Ok(Some(lost(cfg, build, &loss, watch)));
                }
                Seen::Pod(Observed::Building { .. } | Observed::Built(_)) => {
                    return Ok(Some(report::interrupted(
                        "the build pod went on before it was handed the job",
                    )));
                }
            };
            let (deadline, late) = match awaited {
                Awaited::Node(message) => (
                    scheduled_by,
                    outcome::unscheduled_past(timeout, message.as_deref()),
                ),
                Awaited::Stage(where_) => (
                    *started_by.get_or_insert_with(|| Instant::now() + timeout),
                    outcome::unstarted_past(timeout, &where_, &cfg.namespace),
                ),
            };
            tokio::select! {
                changed = status.changed() => changed?,
                () = tokio::time::sleep_until(deadline) => return Ok(Some(late)),
            }
        }
    }

    /// Read what the build left and relay its packages: the report of a
    /// build whose container has ended.
    async fn hand_back<U: Uploads>(
        &self,
        cfg: &Config,
        agent: &Agent<'_, C>,
        build: &Build<'_, U>,
        end: &BuildEnd,
        watch: &StopWatch,
    ) -> CompleteReport {
        let handback = match agent.manifest().await {
            Ok(handback) => Some(handback),
            // An out-of-memory kill is known from the pod's status alone.
            Err(error) if end.oom_killed => {
                tracing::debug!("reading what {} left failed: {error:#}", agent.pod);
                None
            }
            Err(error) => {
                return report::interrupted(format!(
                    "reading what the build left in its pod failed: {error:#}"
                ));
            }
        };
        let (reported, artifacts) = match handback {
            Some(handback) => (handback.report, handback.artifacts),
            None => (None, Vec::new()),
        };
        if reported.is_none() {
            self.last_words(agent.pod, end, &build.log).await;
        }
        let done = complete(
            None,
            watch.elapsed(),
            outcome::ending(end, reported),
            memory(cfg),
        );
        if done.outcome != BuildOutcome::Succeeded {
            return done;
        }
        match deliver(agent, build, &artifacts).await {
            Ok(()) => done,
            Err(error) => {
                let failed = undelivered(done, &error);
                let reason = failed.reason.as_deref().unwrap_or_default();
                say(&build.log, &format!("\n[worker] {reason}\n")).await;
                failed
            }
        }
    }

    /// Copy the last lines the build container wrote, as the kubelet has
    /// them, into the build log: what the agent or the runtime said on the
    /// way down, for a build that left no report to say it.
    async fn last_words(&self, name: &str, end: &BuildEnd, log: &mpsc::Sender<String>) {
        let said = match self.cluster.log_tail(name, BUILD, LAST_LINES).await {
            Ok(said) => said,
            Err(error) => {
                tracing::debug!("reading the last lines of {name} failed: {error:#}");
                String::new()
            }
        };
        let mut text = format!(
            "\n[worker] the build container ended without a report ({})",
            end.detail
        );
        if said.trim().is_empty() {
            text.push('\n');
        } else {
            text.push_str("; the last it wrote:\n");
            text.push_str(said.trim_end());
            text.push('\n');
        }
        say(log, &text).await;
    }

    /// What the cluster says about starting one more build now.
    async fn readiness(&self, cfg: &Config) -> Readiness {
        let waiting = self.cluster.pods().into_iter().find_map(|pod| {
            let message = outcome::unschedulable(&pod)?;
            Some(Readiness::Unschedulable {
                pod: pod.metadata.name.clone().unwrap_or_default(),
                message,
            })
        });
        if let Some(waiting) = waiting {
            return waiting;
        }
        let Ok(probe) = self.representative(cfg) else {
            return Readiness::Ready;
        };
        match self.cluster.admits(&probe).await {
            Admission::Admitted => Readiness::Ready,
            Admission::Refused { message } => Readiness::Refused { message },
            Admission::Unknown { message } => Readiness::Unsure { message },
        }
    }

    /// The pod a build would run in, for asking the cluster whether it
    /// admits one. Admission does not look at where a pod would run, so the
    /// architecture is any this worker can name.
    fn representative(&self, cfg: &Config) -> Result<Pod> {
        let job = JobDescriptor {
            build_id: PROBE_BUILD,
            pkgbase: "admission-check".to_string(),
            arch: "x86_64".to_string(),
            build_flags: Vec::new(),
            persistent_builddir: false,
            makepkg_conf: String::new(),
            pacman_conf: String::new(),
            mirrorlist: MirrorlistOffer::Nothing,
            pgp_keys: Vec::new(),
            vcs_sources: Vec::new(),
            expected_packages: Vec::new(),
            timeout_secs: None,
            expected_version: String::new(),
        };
        pod::render(&PodInputs {
            cfg,
            agent_image: &self.agent_image,
            job: &job,
            timeout: Some(PROBE_TIMEOUT),
        })
    }

    /// Log `found` if it differs from what the last check found.
    fn note(&self, found: &Readiness) {
        let mut last = lock(&self.readiness);
        if *last == *found {
            return;
        }
        match found {
            Readiness::Ready => tracing::info!("Claiming builds again"),
            Readiness::Unsure { message } => tracing::warn!(
                "Could not ask the cluster whether it admits a build pod; claiming builds \
                 anyway: {message}"
            ),
            Readiness::Unschedulable { pod, message } => tracing::warn!(
                "Not claiming builds while pod {pod} has no node to run on: {message}"
            ),
            Readiness::Refused { message } => {
                tracing::warn!("Not claiming builds: the cluster refuses a build pod: {message}");
            }
        }
        *last = found.clone();
    }
}

impl<C: Cluster> Executor for KubeExecutor<C> {
    const KIND: &'static str = "kubernetes";

    async fn run_job(
        &self,
        client: Arc<WorkerClient>,
        claimed: ClaimedJob,
    ) -> Result<CompleteReport> {
        // Once, here: everything this build does reads this copy.
        let cfg = self.cfg.current();
        let build_id = claimed.job.build_id;
        unsupported::note(&claimed.job);
        let pod = self.render(&cfg, &claimed)?;
        let source = client
            .source(build_id)
            .await
            .context("downloading source")?;
        let spec = spec::for_job(&claimed, client.repo(), options(&cfg));
        let input = input::pack(&spec, &source)?;
        let log = BuildLog::open(Arc::clone(&client), build_id);
        let build = Build {
            claimed: &claimed,
            input,
            log: log.sender(),
            uploads: &*client,
        };
        let result = self.run(&cfg, &pod, &build).await;
        // Every other sender is gone with `build`, so this sends what is
        // left and returns.
        drop(build);
        log.close(Vec::new(), None).await;
        result
    }

    /// No claim while the cluster could not start the build: a claimed job
    /// holds a lease, and one whose pod is refused or left pending spends an
    /// attempt for nothing.
    async fn ready_for_work(&self, _claimed: usize) -> bool {
        let cfg = self.cfg.current();
        let found = self.readiness(&cfg).await;
        self.note(&found);
        matches!(found, Readiness::Ready | Readiness::Unsure { .. })
    }

    fn describe_self(&self) -> String {
        let cfg = self.cfg.current();
        format!(
            "kubernetes (namespace {}, builder {})",
            cfg.namespace, cfg.builder_image
        )
    }

    /// Every declared value is taken, for the next build: Kubernetes applies
    /// whatever limit a pod is rendered with, or says at admission that it
    /// will not.
    async fn reconfigure(
        &self,
        settings: WorkerSettings,
        _previous: &WorkerSettings,
    ) -> WorkerSettings {
        self.cfg
            .replace(self.cfg.current().with_settings(&settings));
        settings
    }
}

/// Where a pod is, as last read.
enum Seen {
    /// The watch has not shown the pod yet.
    NotYet,
    Pod(Observed),
    /// The watch showed it, and no longer does.
    Gone,
}

/// One pod's status as the watch has it, read each time the watch moved.
struct Status {
    name: String,
    generation: tokio::sync::watch::Receiver<u64>,
    /// Whether the watch ever showed the pod.
    seen: bool,
    /// The last line written to the build log about it.
    said: Option<String>,
    /// Until when an image that cannot be pulled is waited for, while one
    /// cannot.
    patient_until: Option<Instant>,
}

impl Status {
    fn new(name: &str, generation: tokio::sync::watch::Receiver<u64>) -> Self {
        Self {
            name: name.to_string(),
            generation,
            seen: false,
            said: None,
            patient_until: None,
        }
    }

    /// Where the pod is now, said in the build log when that changed.
    async fn read<C: Cluster>(&mut self, cluster: &C, log: &mpsc::Sender<String>) -> Seen {
        let Some(pod) = cluster.pod(&self.name) else {
            return if self.seen { Seen::Gone } else { Seen::NotYet };
        };
        self.seen = true;
        let observed = self.patient(outcome::observe(&pod));
        let line = observed.line();
        if line.is_some() && line != self.said {
            if let Some(line) = &line {
                say(log, &format!("[worker] {line}\n")).await;
            }
            self.said = line;
        }
        Seen::Pod(observed)
    }

    /// `observed`, with an image that cannot be pulled turned into a loss
    /// once it has been waited for long enough.
    fn patient(&mut self, observed: Observed) -> Observed {
        let Observed::Unpulled { message } = observed else {
            self.patient_until = None;
            return observed;
        };
        let until = *self
            .patient_until
            .get_or_insert_with(|| Instant::now() + PULL_PATIENCE);
        if Instant::now() < until {
            Observed::Unpulled { message }
        } else {
            Observed::Lost(outcome::Loss::Stuck { message })
        }
    }

    /// Resolves when there is something new to read: the watch has moved
    /// since the last read, or the wait for an image is over.
    async fn changed(&mut self) -> Result<()> {
        let patience = async {
            match self.patient_until {
                Some(until) => tokio::time::sleep_until(until).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            changed = self.generation.changed() => {
                changed.map_err(|_| anyhow!("the watch on build pods ended"))
            }
            () = patience => Ok(()),
        }
    }
}

/// Whether `pod`, one of this dispatcher's, is nobody's: a build's that no
/// build in `held` is, or one of no build that has ended. A pod already
/// being deleted is left to that.
fn orphaned(pod: &Pod, held: &HashSet<i32>) -> bool {
    if pod.metadata.deletion_timestamp.is_some() {
        return false;
    }
    match pod::build_id(pod) {
        Some(build_id) => !held.contains(&build_id),
        None => {
            let phase = pod
                .status
                .as_ref()
                .and_then(|status| status.phase.as_deref());
            matches!(phase, Some("Succeeded" | "Failed"))
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Write to the build log. Losing a line is not worth failing a build over.
async fn say(log: &mpsc::Sender<String>, text: &str) {
    let _ = log.send(text.to_string()).await;
}

/// The report of a build whose pod can no longer produce or hand back one.
fn lost<U>(
    cfg: &Config,
    build: &Build<'_, U>,
    loss: &outcome::Loss,
    watch: &StopWatch,
) -> CompleteReport {
    outcome::lost(
        loss,
        watch.elapsed(),
        build.claimed.timeout,
        cfg.tunables.disk,
    )
}

/// The report of a build this worker stopped.
fn stopped(stop: Stop, watch: &StopWatch) -> CompleteReport {
    let ending = Ending::Unreported {
        detail: "the pod was deleted".to_string(),
    };
    complete(Some(stop), watch.elapsed(), ending, None)
}

/// The worker's own memory limit, for naming in an out-of-memory failure.
fn memory(cfg: &Config) -> Option<MemoryLimit> {
    cfg.tunables.memory.map(|bytes| MemoryLimit {
        bytes,
        variable: MEMORY_VARIABLE,
    })
}

/// Relay every package the manifest lists. None at all is the package's
/// failure, as on every worker.
async fn deliver<C: Cluster, U: Uploads>(
    agent: &Agent<'_, C>,
    build: &Build<'_, U>,
    artifacts: &[Artifact],
) -> Result<()> {
    if artifacts.is_empty() {
        return Err(PackagesRefused("build produced no artifacts".into()).into());
    }
    // Before any of them is sent: a manifest with a name that is not a
    // package's is not one the agent wrote.
    if let Some(odd) = artifacts
        .iter()
        .find(|artifact| !is_artifact(&artifact.name))
    {
        bail!(
            "the pod's manifest names {:?}, which is not a package",
            odd.name
        );
    }
    let build_id = build.claimed.job.build_id;
    for artifact in artifacts {
        say(
            &build.log,
            &format!("[worker] uploading {}\n", artifact.name),
        )
        .await;
        agent
            .relay(build.uploads, build_id, artifact)
            .await
            .with_context(|| format!("uploading {}", artifact.name))?;
    }
    Ok(())
}

/// The report of a build that succeeded and whose packages did not reach the
/// server, keeping what the build measured.
fn undelivered(done: CompleteReport, error: &anyhow::Error) -> CompleteReport {
    CompleteReport {
        peak_memory_bytes: done.peak_memory_bytes,
        disk_usage: done.disk_usage,
        vcs_commits: done.vcs_commits,
        ..report::from_error(error)
    }
}

/// What the worker decides about a build's spec: the limits it runs under,
/// and where in the pod its trees and its credential are.
fn options(cfg: &Config) -> SpecOptions {
    SpecOptions {
        cpus: cfg.tunables.cpus,
        keyserver: cfg.tunables.keyserver.clone(),
        scratch: Path::new(pod::WORK).join(SCRATCH),
        credential: pod::credentials(cfg).map(|mount| Credential {
            key: mount.join(CREDENTIAL_KEY),
            // Always named: the dispatcher cannot read the Secret to know
            // whether it holds one, and a build without it is refused. A pod
            // is new each time, so an ssh left to trust the keys it is shown
            // would trust any host, on every build.
            known_hosts: Some(mount.join(CREDENTIAL_KNOWN_HOSTS)),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentImage, PodExtras, Tunables};

    fn cfg(credential_secret: Option<&str>) -> Config {
        Config {
            namespace: "aurcache-builds".to_string(),
            dispatcher: "0123456789abcdef".to_string(),
            builder_image: "builder:1".to_string(),
            pull_policy: "Always".to_string(),
            agent_image: AgentImage::Given("agent:1".to_string()),
            credential_secret: credential_secret.map(str::to_string),
            user_namespaces: false,
            extras: PodExtras::default(),
            network_probe: false,
            tunables: Tunables {
                memory: Some(4 << 30),
                cpus: Some(2.5),
                disk: None,
                schedule_timeout: Duration::from_secs(600),
                keyserver: "hkps://keys.example".to_string(),
            },
        }
    }

    fn rendered(cfg: &Config) -> Pod {
        struct Unused;
        impl Cluster for Unused {
            async fn create(&self, _: &Pod) -> Result<(), CreateError> {
                unreachable!()
            }
            async fn admits(&self, _: &Pod) -> Admission {
                unreachable!()
            }
            async fn delete(&self, _: &str, _: Duration) -> Result<()> {
                unreachable!()
            }
            async fn sweep(&self) -> Result<usize> {
                unreachable!()
            }
            fn pod(&self, _: &str) -> Option<Arc<Pod>> {
                unreachable!()
            }
            fn pods(&self) -> Vec<Arc<Pod>> {
                unreachable!()
            }
            fn generation(&self) -> tokio::sync::watch::Receiver<u64> {
                unreachable!()
            }
            async fn exec(
                &self,
                _: &str,
                _: &str,
                _: &[&str],
                _: crate::cluster::Stdin,
            ) -> Result<crate::cluster::Exec> {
                unreachable!()
            }
            async fn log_tail(&self, _: &str, _: &str, _: i64) -> Result<String> {
                unreachable!()
            }
        }
        KubeExecutor::new(cfg.clone(), Arc::new(Unused), "agent:1".to_string())
            .representative(cfg)
            .unwrap()
    }

    #[test]
    fn the_spec_takes_the_limits_and_the_scratch_in_the_work_volume() {
        let cfg = cfg(None);
        let options = options(&cfg);
        assert_eq!(options.cpus, Some(2.5));
        assert_eq!(options.keyserver, "hkps://keys.example");
        assert_eq!(options.scratch, Path::new("/work/scratch"));
        assert_eq!(options.credential, None);
    }

    /// The credential is named where the build container has the Secret,
    /// host keys included whatever the Secret holds.
    #[test]
    fn the_spec_names_the_credential_where_the_pod_mounts_it() {
        let cfg = cfg(Some("build-ssh"));
        let mount = pod::credentials(&cfg).unwrap();
        let credential = options(&cfg).credential.unwrap();
        assert_eq!(credential.key, mount.join("ssh-privatekey"));
        assert_eq!(credential.known_hosts, Some(mount.join("known_hosts")));
    }

    #[test]
    fn the_admission_check_asks_about_a_pod_no_build_has() {
        let cfg = cfg(None);
        let pod = rendered(&cfg);
        assert_eq!(
            pod.metadata.name.as_deref(),
            Some("aurcache-build-0123456789abcdef-0")
        );
        let spec = pod.spec.unwrap();
        assert!(spec.active_deadline_seconds.is_some());
    }

    #[test]
    fn an_out_of_memory_failure_names_the_workers_limit() {
        let limit = memory(&cfg(None)).unwrap();
        assert_eq!(limit.bytes, 4 << 30);
        assert_eq!(limit.variable, "WORKER_BUILD_MEMORY_MAX");
    }

    #[test]
    fn packages_that_did_not_arrive_keep_the_builds_measurements() {
        let done = CompleteReport {
            peak_memory_bytes: Some(7),
            ..report::success()
        };
        let lost = undelivered(done.clone(), &anyhow!("connection reset"));
        assert_eq!(lost.outcome, BuildOutcome::Interrupted);
        assert_eq!(lost.peak_memory_bytes, Some(7));
        let refused = undelivered(done, &PackagesRefused("too large".into()).into());
        assert_eq!(refused.outcome, BuildOutcome::Failed);
    }
}
