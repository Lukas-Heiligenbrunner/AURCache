//! The lifecycle of one build: a claimed job becomes a pod, a build log,
//! streamed packages and one report.
//!
//! In order: the pod is created; the job is handed in once its `stage`
//! container runs; the log is followed while the build container runs; when
//! that container has ended, what it left is read, the log is read to the
//! length it gives, and each package is relayed to the server. Whatever
//! happens on the way, the pod is deleted before the report is returned.

use crate::cluster::{Cluster, CreateError, Sweep};
use crate::config::Config;
use crate::outcome::{self, BuildEnd, Observed};
use crate::pod::{self, BUILD, PodInputs};
use crate::transfer::{Agent, Lines, Uploads};
use anyhow::{Context, Result, anyhow};
use aurcache_build_agent::layout::Scratch;
use aurcache_build_agent::report::{Artifact, Handback};
use aurcache_build_agent::spec::SshAccess;
use aurcache_common::worker::{BuildOutcome, CompleteReport, JobDescriptor, MirrorlistOffer};
use aurcache_worker_container::input::{self, Packed};
use aurcache_worker_container::outcome::{Ending, MemoryLimit, complete};
use aurcache_worker_container::spec::{self, SpecOptions};
use aurcache_worker_container::ssh::{self, Credential};
use aurcache_worker_core::artifacts::is_artifact;
use aurcache_worker_core::build_log::BuildLog;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::executor::{ClaimedJob, Executor};
use aurcache_worker_core::live::Live;
use aurcache_worker_core::protocol::{self, PackagesRefused, Stop, StopWatch};
use aurcache_worker_core::report;
use aurcache_worker_core::settings::WorkerSettings;
use aurcache_worker_core::ssh_key;
use futures::FutureExt;
use futures::future::{BoxFuture, Fuse, FusedFuture};
use k8s_openapi::api::core::v1::Pod;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// How long a pod's containers are given to end when it is deleted.
const GRACE: Duration = Duration::from_secs(5);
/// How long to wait before trying a failed delete again.
const DELETE_RETRY: Duration = Duration::from_secs(5);
/// How long the rest of the log may take to arrive once the build container
/// has ended.
const LOG_END: Duration = Duration::from_secs(60);
/// How many of the build container's last lines are copied into the build
/// log when it ended without a report.
const LAST_LINES: i64 = 20;
/// Where the build's own trees go, under the work volume, so that everything
/// a build writes is in the volume its disk limit is on.
const SCRATCH: &str = "scratch";
/// The build that the startup dry run asks about: an id no build has.
const PROBE_BUILD: i32 = 0;

/// Builds each job in a pod of its own.
pub struct KubeExecutor<C: Cluster> {
    /// The configuration in force. Each build takes the one current when it
    /// starts and keeps it, so a pod is never rendered under one limit and
    /// reported under another.
    cfg: Live<Config>,
    cluster: Arc<C>,
    /// The pod that last held claims back, so that a change is logged once.
    waiting: Mutex<Option<String>>,
}

/// What one build is made from, beside the configuration.
pub struct Build<'a, U> {
    pub claimed: &'a ClaimedJob,
    /// The job as `receive` reads it, whose second piece is the source as it
    /// was downloaded. Taken out when the build starts, and dropped once the
    /// pod has it.
    pub input: Packed,
    /// Where the build log goes, a chunk of text at a time.
    pub log: mpsc::Sender<String>,
    pub uploads: &'a U,
    /// The socket of the ssh-agent that has the SSH key the build fetches
    /// its sources with, which the link leads to; `None` when the dispatcher
    /// has no key. Whoever starts the build keeps that agent for as long as
    /// the build runs.
    pub ssh: Option<PathBuf>,
}

/// How far a build got before something other than its end decided it.
enum Reached {
    /// The build container ended; what it left is in the pod.
    Built(BuildEnd),
    /// The build is over with this report, and nothing to hand back.
    Over(CompleteReport),
}

impl<C: Cluster> KubeExecutor<C> {
    pub fn new(cfg: Config, cluster: Arc<C>) -> Self {
        Self {
            cfg: Live::new(cfg),
            cluster,
            waiting: Mutex::new(None),
        }
    }

    /// Delete every build pod in the builds' namespace, whichever dispatcher
    /// made it: pods that a previous process left, whose builds the server
    /// gives up as soon as this one says it holds none, and pods of a
    /// dispatcher whose identity is gone, which nobody else will ever collect.
    /// A builds namespace is one dispatcher's. How many there were.
    pub async fn sweep(&self) -> Result<usize> {
        let swept = self
            .cluster
            .sweep(Sweep::Namespace)
            .await
            .context("deleting the build pods a previous run left")?;
        if swept > 0 {
            tracing::warn!(
                "Deleted {swept} build pod(s) left by a previous run of this worker, or of a \
                 worker with another identity, in its builds namespace; the server ends their \
                 builds, and queues a new build of each package while it has attempts left"
            );
        }
        Ok(swept)
    }

    /// Ask the cluster, with a dry run, whether it admits a build pod: a
    /// namespace, a Pod Security level or a policy that refuses every build
    /// stops the worker here, before it claims anything.
    pub async fn check_admission(&self) -> Result<()> {
        let probe = probe(&self.cfg.current())?;
        self.cluster
            .admits(&probe)
            .await
            .context("the cluster does not admit a build pod")
    }

    /// Build one job whose input is packed: everything [`Executor::run_job`]
    /// does once it has the job's source.
    pub async fn build<U: Uploads>(&self, build: Build<'_, U>) -> Result<CompleteReport> {
        let cfg = self.cfg.current();
        let pod = render(&cfg, build.claimed)?;
        self.run(&cfg, &pod, build).await
    }

    /// Create the pod, see the build through in it, and delete it. A panic
    /// leaves the pod to its deadline: the runner reports the build.
    async fn run<U: Uploads>(
        &self,
        cfg: &Config,
        pod: &Pod,
        mut build: Build<'_, U>,
    ) -> Result<CompleteReport> {
        let watch = build.claimed.watch();
        let name = pod::name(&cfg.dispatcher, build.claimed.job.build_id);
        let input = std::mem::take(&mut build.input);
        let lines = Mutex::new(Lines::default());
        let attended = self
            .attend(cfg, pod, &name, &build, input, &lines, &watch)
            .await;
        flush(&lines, &build.log).await;
        self.delete(&name).await;
        attended
    }

    /// Delete a build's pod, trying once more when that fails. A pod left
    /// standing holds its place in a quota until its deadline.
    async fn delete(&self, name: &str) {
        for retry in [Some(DELETE_RETRY), None] {
            match self.cluster.delete(name, GRACE).await {
                Ok(()) => return tracing::debug!("deleted pod {name}"),
                Err(error) => tracing::warn!("{error:#}"),
            }
            if let Some(pause) = retry {
                tokio::time::sleep(pause).await;
            }
        }
    }

    /// The build from pod creation to its report, stopped where the job's
    /// cancellation or timeout says.
    ///
    /// A timeout bounds the build itself: once the build container has ended
    /// it no longer applies, and only a cancellation stops the hand-back.
    #[allow(clippy::too_many_arguments)]
    async fn attend<U: Uploads>(
        &self,
        cfg: &Config,
        pod: &Pod,
        name: &str,
        build: &Build<'_, U>,
        input: Packed,
        lines: &Mutex<Lines>,
        watch: &StopWatch,
    ) -> Result<CompleteReport> {
        // A build stopped before it has a pod gets none.
        if build.claimed.cancel.is_cancelled() {
            return Ok(stopped(Stop::Canceled, watch));
        }
        // Not raced against a stop: a request to create a pod that is dropped
        // half-way can still be answered, after the delete meant to undo it,
        // and the pod would then stand until its deadline.
        match self.cluster.create(pod).await {
            Ok(()) => {}
            Err(CreateError::Refused { message }) => return Ok(outcome::refused(&message)),
            Err(CreateError::Other(error)) => return Err(error),
        }
        say(&build.log, &format!("[worker] created pod {name}\n")).await;

        let bin = pod::agent_path();
        let agent = Agent {
            cluster: &*self.cluster,
            pod: name,
            bin: &bin,
        };
        let reached = tokio::select! {
            biased;
            reached = self.until_built(cfg, &agent, build, input, lines, watch) => reached?,
            stop = watch.stopped() => return Ok(stopped(stop, watch)),
        };
        let end = match reached {
            Reached::Over(report) => return Ok(report),
            Reached::Built(end) => end,
        };
        tokio::select! {
            biased;
            report = self.hand_back(cfg, &agent, build, &end, lines, watch) => Ok(report),
            () = build.claimed.cancel.cancelled() => Ok(stopped(Stop::Canceled, watch)),
        }
    }

    /// Hand the job to the pod, once it can take it, and follow the build
    /// until its container has ended or the pod is lost.
    ///
    /// The build must run within the schedule timeout of the pod's creation:
    /// waiting for a node, pulling the images and taking the job are none of
    /// them the build's own time, and a pod that cannot start (an image that
    /// will not pull, a volume that will not mount) often says so only in
    /// events.
    async fn until_built<U: Uploads>(
        &self,
        cfg: &Config,
        agent: &Agent<'_, C>,
        build: &Build<'_, U>,
        input: Packed,
        lines: &Mutex<Lines>,
        watch: &StopWatch,
    ) -> Result<Reached> {
        let log = &build.log;
        let timeout = cfg.tunables.schedule_timeout;
        let started_by = Instant::now() + timeout;
        let unstarted =
            |last: &str| Reached::Over(outcome::unstarted(timeout, last, &cfg.namespace));
        let mut status = Status::new(agent.pod, self.cluster.generation());
        if let Some(over) = self
            .until_staged(cfg, build, &mut status, started_by, watch)
            .await?
        {
            return Ok(over);
        }
        say(log, "[worker] handing the job to the pod\n").await;
        // Once, and never again: a second hand-in is refused by the agent,
        // and after a failed one nobody knows whether a build started.
        let handed = tokio::time::timeout_at(started_by, agent.receive(&input)).await;
        // The pod has it now, or never will: the build may run for hours.
        drop(input);
        match handed {
            Err(_) => return Ok(unstarted("the job was being handed to the pod")),
            Ok(Err(error)) => {
                return Ok(Reached::Over(report::interrupted(format!(
                    "handing the job to the build pod failed: {error:#}"
                ))));
            }
            Ok(Ok(())) => {}
        }

        let mut started = false;
        let follow = agent.follow(lines, log);
        tokio::pin!(follow);
        // The SSH link, opened once the build container runs, for the
        // whole build. It ends with the build container.
        let mut link: Fuse<BoxFuture<'_, Result<()>>> = Fuse::terminated();
        loop {
            match status.read(&*self.cluster, log).await {
                Seen::Gone => return Ok(Reached::Over(outcome::gone())),
                Seen::Pod(Observed::Built(end)) => return Ok(Reached::Built(end)),
                Seen::Pod(Observed::Lost(loss)) => {
                    return Ok(Reached::Over(lost(cfg, build, &loss, watch)));
                }
                Seen::NotYet | Seen::Pod(_) => {}
            }
            if !started
                && self
                    .cluster
                    .pod(agent.pod)
                    .is_some_and(|pod| build_runs(&pod))
            {
                started = true;
                if let Some(ssh_agent) = build.ssh.clone() {
                    let socket = Scratch::new(scratch()).ssh_link();
                    link = async move { agent.ssh_link(&ssh_agent, &socket).await }
                        .boxed()
                        .fuse();
                }
            }
            // The link first, so that it is opened before anything else
            // this turn can end the build.
            tokio::select! {
                biased;
                served = &mut link, if !link.is_terminated() => if let Err(error) = served {
                    tracing::warn!("build {}: the SSH link: {error:#}", build.claimed.job.build_id);
                },
                changed = status.changed() => changed?,
                never = &mut follow => match never {},
                () = tokio::time::sleep_until(started_by), if !started => {
                    return Ok(unstarted(&status.said.clone().unwrap_or_default()));
                }
            }
        }
    }

    /// Wait for the pod's `stage` container to run, which is when the job
    /// can be handed in. `Some` is the end of a build that ended here.
    async fn until_staged<U: Uploads>(
        &self,
        cfg: &Config,
        build: &Build<'_, U>,
        status: &mut Status,
        started_by: Instant,
        watch: &StopWatch,
    ) -> Result<Option<Reached>> {
        loop {
            let over = match status.read(&*self.cluster, &build.log).await {
                Seen::Gone => outcome::gone(),
                Seen::Pod(Observed::Staged) => return Ok(None),
                Seen::Pod(Observed::Lost(loss)) => lost(cfg, build, &loss, watch),
                Seen::Pod(Observed::Building { .. } | Observed::Built(_)) => {
                    report::interrupted("the build pod went on before it was handed the job")
                }
                Seen::NotYet
                | Seen::Pod(Observed::Unscheduled { .. } | Observed::Starting { .. }) => {
                    tokio::select! {
                        changed = status.changed() => changed?,
                        () = tokio::time::sleep_until(started_by) => {
                            let last = status.said.clone().unwrap_or_default();
                            return Ok(Some(Reached::Over(outcome::unstarted(
                                cfg.tunables.schedule_timeout,
                                &last,
                                &cfg.namespace,
                            ))));
                        }
                    }
                    continue;
                }
            };
            return Ok(Some(Reached::Over(over)));
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
        lines: &Mutex<Lines>,
        watch: &StopWatch,
    ) -> CompleteReport {
        let Handback {
            report: reported,
            log_size,
            mut artifacts,
            ..
        } = match agent.manifest().await {
            Ok(handback) => handback,
            Err(error) => {
                return report::interrupted(format!(
                    "reading what the build left in its pod failed: {error:#}"
                ));
            }
        };
        let log = &build.log;
        if tokio::time::timeout(LOG_END, agent.follow_to(lines, log, log_size))
            .await
            .is_err()
        {
            tracing::warn!(
                "build {}: the log of {} did not reach its end; reporting what arrived",
                build.claimed.job.build_id,
                agent.pod
            );
        }
        flush(lines, log).await;
        if reported.is_none() {
            self.last_words(agent.pod, end, log).await;
        }
        let done = complete(
            None,
            watch.elapsed(),
            outcome::ending(end, reported),
            memory_limit(cfg),
        );
        if done.outcome != BuildOutcome::Succeeded {
            return done;
        }
        // Only these names ever reach a URL or the pod again.
        artifacts.retain(|artifact| is_artifact(&artifact.name));
        match deliver(agent, build, &artifacts).await {
            Ok(()) => done,
            Err(error) => {
                let failed = report::from_error(&error);
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
        let pod = render(&cfg, &claimed)?;
        // Decided before anything is prepared and said first in the build
        // log, so a fetch that fails for want of a key has the reason above
        // it. This worker holds no named key, so none is ever withheld.
        let held = ssh::held(cfg.credential.as_ref());
        let offer = ssh_key::offer(&claimed.job, &held, true);
        if let Some(notice) = offer.notice(&claimed.job.pkgbase) {
            protocol::log(&client, build_id, &format!("[worker] {notice}\n")).await;
        }
        let source = client
            .source(build_id)
            .await
            .context("downloading source")?;
        // An ssh-agent of this build's own, started from the files as they
        // are now, so that a key or host keys changed in the Secret are
        // used from the next build on. It is killed when this returns.
        let credential: Option<Credential> = match offer.key() {
            Some(files) => match files.load().await {
                Ok(credential) => Some(credential),
                Err(error) => {
                    return Ok(report::interrupted(format!(
                        "the worker's SSH key could not be used: {error:#}"
                    )));
                }
            },
            None => None,
        };
        let ssh = credential.as_ref().map(|credential| SshAccess {
            known_hosts: credential.known_hosts.clone(),
        });
        let ssh_agent = credential
            .as_ref()
            .map(|credential| credential.agent.socket().to_path_buf());
        let spec = spec::for_job(&claimed, client.repo(), options(&cfg, ssh));
        // The source is moved into the input, not copied: it is held once,
        // until the pod has it.
        let input = input::pack(&spec, source)?;
        let log = BuildLog::open(Arc::clone(&client), build_id);
        let build = Build {
            claimed: &claimed,
            input,
            log: log.sender(),
            uploads: &*client,
            ssh: ssh_agent,
        };
        // Every other sender goes with `build`, so closing the log sends
        // what is left and returns.
        let result = self.run(&cfg, &pod, build).await;
        log.close(Vec::new(), None).await;
        result
    }

    /// No claim while a pod of this dispatcher's has no node to go to: a
    /// claim cannot name an architecture, so the next build would wait for
    /// a node too, and its lease would run out meanwhile.
    async fn ready_for_work(&self, _claimed: usize) -> bool {
        let found = self.cluster.pods().into_iter().find_map(|pod| {
            let message = outcome::unschedulable(&pod)?;
            Some((pod.metadata.name.clone().unwrap_or_default(), message))
        });
        let mut last = lock(&self.waiting);
        let now = found.as_ref().map(|(pod, _)| pod.clone());
        if *last != now {
            match &found {
                Some((pod, message)) => tracing::warn!(
                    "Not claiming builds while pod {pod} has no node to run on: {message}"
                ),
                None => tracing::info!("Claiming builds again"),
            }
            *last = now;
        }
        found.is_none()
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
}

impl Status {
    fn new(name: &str, generation: tokio::sync::watch::Receiver<u64>) -> Self {
        Self {
            name: name.to_string(),
            generation,
            seen: false,
            said: None,
        }
    }

    /// Where the pod is now, said in the build log when that changed.
    async fn read<C: Cluster>(&mut self, cluster: &C, log: &mpsc::Sender<String>) -> Seen {
        let Some(pod) = cluster.pod(&self.name) else {
            return if self.seen { Seen::Gone } else { Seen::NotYet };
        };
        self.seen = true;
        let observed = outcome::observe(&pod);
        let line = observed.line();
        if line.is_some() && line != self.said {
            if let Some(line) = &line {
                say(log, &format!("[worker] {line}\n")).await;
            }
            self.said = line;
        }
        Seen::Pod(observed)
    }

    /// Resolves when the watch has moved since the last read.
    async fn changed(&mut self) -> Result<()> {
        self.generation
            .changed()
            .await
            .map_err(|_| anyhow!("the watch on build pods ended"))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Pass on what the log holds back for a line end, ending its line, so that
/// what the dispatcher says next starts a line of its own.
async fn flush(lines: &Mutex<Lines>, log: &mpsc::Sender<String>) {
    let rest = lock(lines).flush();
    if let Some(mut rest) = rest {
        if !rest.ends_with('\n') {
            rest.push('\n');
        }
        say(log, &rest).await;
    }
}

/// Write to the build log. Losing a line is not worth failing a build over.
async fn say(log: &mpsc::Sender<String>, text: &str) {
    let _ = log.send(text.to_string()).await;
}

/// The pod for `claimed`.
fn render(cfg: &Config, claimed: &ClaimedJob) -> Result<Pod> {
    pod::render(&PodInputs {
        cfg,
        job: &claimed.job,
        timeout: claimed.timeout.map(|timeout| timeout.limit),
    })
}

/// The pod a build would run in, for the startup dry run. Admission does
/// not look at where a pod would run, so the architecture is any this worker
/// can name.
fn probe(cfg: &Config) -> Result<Pod> {
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
        ssh_key: None,
    };
    pod::render(&PodInputs {
        cfg,
        job: &job,
        timeout: None,
    })
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

/// The memory limit this worker set, for naming in an out-of-memory failure.
fn memory_limit(cfg: &Config) -> Option<MemoryLimit> {
    Some(MemoryLimit {
        bytes: cfg.tunables.memory?,
        variable: "WORKER_BUILD_MEMORY_MAX",
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

/// What the worker decides about a build's spec: the limits it runs under,
/// where in the pod its trees are, and the hosts its SSH key is used with.
fn options(cfg: &Config, ssh: Option<SshAccess>) -> SpecOptions {
    SpecOptions {
        cpus: cfg.tunables.cpus,
        keyserver: cfg.tunables.keyserver.clone(),
        scratch: scratch(),
        ssh,
    }
}

/// Where the build's own trees are, in the pod.
fn scratch() -> PathBuf {
    Path::new(pod::WORK).join(SCRATCH)
}

/// Whether the build container runs, which is when it can be exec'd into.
fn build_runs(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|status| status.container_statuses.as_ref())
        .into_iter()
        .flatten()
        .any(|container| {
            container.name == BUILD
                && container
                    .state
                    .as_ref()
                    .is_some_and(|state| state.running.is_some())
        })
}

/// Delete this dispatcher's build pods still there as it stops: those whose
/// build did not get as far as deleting them. Another dispatcher's are left
/// alone, though they share the namespace. How many there were.
pub async fn leave<C: Cluster>(cluster: &C) -> Result<usize> {
    cluster
        .sweep(Sweep::Own)
        .await
        .context("deleting the pods left as this worker stops")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PodExtras, Tunables};

    fn cfg() -> Config {
        Config {
            namespace: "aurcache-builds".to_string(),
            dispatcher: "0123456789abcdef".to_string(),
            builder_image: "builder:1".to_string(),
            pull_policy: "Always".to_string(),
            agent_image: "agent:1".to_string(),
            credential: None,
            extras: PodExtras::default(),
            tunables: Tunables {
                memory: Some(4 << 30),
                cpus: Some(2.5),
                disk: None,
                schedule_timeout: Duration::from_secs(600),
                keyserver: "hkps://keys.example".to_string(),
            },
        }
    }

    /// The spec takes the limits, and the scratch in the work volume, under
    /// which the agent listens for the link.
    #[test]
    fn the_spec_takes_the_limits_and_the_scratch_in_the_work_volume() {
        let options = options(&cfg(), None);
        assert_eq!(options.cpus, Some(2.5));
        assert_eq!(options.keyserver, "hkps://keys.example");
        assert_eq!(options.scratch, Path::new("/work/scratch"));
        assert_eq!(options.ssh, None);
        assert_eq!(
            Scratch::new(options.scratch).ssh_link(),
            Path::new("/work/scratch/ssh-link/link.sock")
        );
    }

    #[test]
    fn the_dry_run_asks_about_a_pod_no_build_has() {
        let pod = probe(&cfg()).unwrap();
        assert_eq!(
            pod.metadata.name.as_deref(),
            Some("aurcache-build-0123456789abcdef-0")
        );
        assert!(pod.spec.unwrap().active_deadline_seconds.is_some());
    }
}
