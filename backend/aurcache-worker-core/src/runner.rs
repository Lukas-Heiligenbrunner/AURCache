//! The long-lived worker loop: claim jobs up to the concurrency limit, run each
//! in its own task with panic-safe completion, and heartbeat liveness. If the
//! server becomes unreachable for longer than the lease TTL, in-flight builds
//! self-abort (the server will have abandoned them and queued fresh ones).
//!
//! The heartbeat is also how values set on the server reach the worker: its
//! answer carries a snapshot whenever the worker does not hold the current one,
//! and the worker takes it in without restarting -- see [`Runner::apply`].

use anyhow::Result;
use aurcache_common::worker::{
    BuildOutcome, ClaimRequest, CompleteReport, Heartbeat, JobDescriptor, MirrorlistOffer,
    MirrorlistPreference,
};
use aurcache_common::worker_config::{ConfigSnapshot, EffectiveConfig};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::client::{Lease, Refused, WorkerClient};
use crate::config::CoreConfig;
use crate::executor::{ClaimedJob, Executor};
use crate::gate::ConcurrencyGate;
use crate::live::Live;
use crate::protocol::{Timeout, log};
use crate::report;

/// The fields of a registration the server schedules by.
///
/// Registration carries the worker's concurrency, priority and package
/// affinity, which the server's scheduler reads from the worker's row. When
/// delivered values change any of them, the worker registers again with what
/// it now runs, so neither end has to know that the `concurrency` setting and
/// the registered concurrency are the same thing.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Routing {
    packages: Vec<String>,
    priority: i32,
    concurrency: usize,
}

impl Routing {
    fn of(cfg: &CoreConfig) -> Self {
        Self {
            packages: cfg.tunables.packages.clone(),
            priority: cfg.tunables.priority,
            concurrency: cfg.tunables.concurrency,
        }
    }
}

/// Shared runtime state for the worker loop.
pub struct Runner<E: Executor> {
    /// The configuration in force: a loop iteration or a job works with one
    /// copy from start to end.
    cfg: Live<CoreConfig>,
    client: Arc<WorkerClient>,
    executor: Arc<E>,
    /// Build ids claimed and not yet reported (reported in each heartbeat),
    /// with what stops each.
    active: Mutex<HashMap<i32, CancellationToken>>,
    /// Unix seconds of the last successful server contact.
    last_contact: AtomicU64,
    /// Bounds concurrent builds, and follows the `concurrency` setting.
    gate: Arc<ConcurrencyGate>,
    /// Mirrorlists the server has sent, by architecture. The checksums go out with each claim so the server can skip
    /// resending what has not changed; the content is what a job then uses.
    ///
    /// Memory only. Losing it on restart costs one resend, and a restart is
    /// exactly when a worker should re-read the deployment's configuration
    /// anyway.
    mirrorlists: Mutex<BTreeMap<String, HeldMirrorlist>>,
    /// What the server has been told this worker's settings resolved to.
    ///
    /// The report rides the heartbeat and is sent only when it differs from
    /// this, so a fleet at rest costs one report per worker process rather than
    /// one every heartbeat. `None` until the first accepted heartbeat, which is
    /// what makes a restarted worker tell a server that has forgotten it.
    reported_config: Mutex<Option<EffectiveConfig>>,
    /// The scheduling fields the server last accepted a registration with,
    /// so a delivery that changes them is followed by registering again -- and
    /// one that fails is retried on the next heartbeat rather than forgotten.
    registered: Mutex<Routing>,
}

/// A mirrorlist the server sent, kept for the jobs it withholds it from.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeldMirrorlist {
    /// The server's checksum of `content`, advertised with each claim.
    checksum: String,
    content: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// How many times a completion is sent before the lease reaper is left to it.
const COMPLETION_ATTEMPTS: u32 = 5;

/// What to do after a completion report failed.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// Send it again after this long.
    Retry(Duration),
    /// The server refused it, which no retry changes: the build is gone, or no
    /// longer this worker's. Holding the build's slot to ask again would only
    /// keep the next build waiting.
    Refused,
    /// Out of attempts.
    Exhausted,
}

/// The decision after `attempt` (counted from 1) failed with `error`.
fn after_failure(error: &anyhow::Error, attempt: u32) -> Next {
    if error
        .downcast_ref::<Refused>()
        .is_some_and(Refused::is_final)
    {
        Next::Refused
    } else if attempt < COMPLETION_ATTEMPTS {
        Next::Retry(Duration::from_secs(2 * u64::from(attempt)))
    } else {
        Next::Exhausted
    }
}

/// The mirrorlist a build of `arch` uses, given what the server offered, and
/// `held` kept in step with it.
///
/// A free function rather than a method so the rule can be tested without a
/// server, a client or an executor -- and so there is exactly one copy of it.
///
/// Precedence: the worker's own mirrorlist, then the server's for this arch,
/// then nothing at all (the image's `/etc/pacman.d/mirrorlist`).
fn resolve_mirrorlist(
    local: Option<&str>,
    held: &mut BTreeMap<String, HeldMirrorlist>,
    arch: &str,
    offer: &MirrorlistOffer,
) -> Option<String> {
    if let Some(local) = local {
        return Some(local.to_string());
    }
    match offer {
        // Sent afresh: use it, and remember it for the next claim.
        MirrorlistOffer::Send { content, checksum } => {
            held.insert(
                arch.to_string(),
                HeldMirrorlist {
                    checksum: checksum.clone(),
                    content: content.clone(),
                },
            );
            Some(content.clone())
        }
        // Withheld because we already hold it.
        MirrorlistOffer::Keep => held.get(arch).map(|h| h.content.clone()),
        // The server has none for this arch. Drop anything stale rather than
        // reuse it, or a mirrorlist removed on the server would live on here.
        MirrorlistOffer::Nothing => {
            held.remove(arch);
            None
        }
    }
}

impl<E: Executor> Runner<E> {
    /// `cfg` must be what the worker registered with at enrollment, which is
    /// what the server is taken to hold until this registers again; `client`
    /// is the one enrollment returned, which holds that registration.
    pub fn new(cfg: CoreConfig, client: WorkerClient, executor: E) -> Arc<Self> {
        Arc::new(Self {
            gate: ConcurrencyGate::new(cfg.tunables.concurrency),
            registered: Mutex::new(Routing::of(&cfg)),
            cfg: Live::new(cfg),
            client: Arc::new(client),
            executor: Arc::new(executor),
            active: Mutex::new(HashMap::new()),
            last_contact: AtomicU64::new(now_secs()),
            mirrorlists: Mutex::new(BTreeMap::new()),
            reported_config: Mutex::new(None),
        })
    }

    /// The configuration in force now.
    fn cfg(&self) -> Arc<CoreConfig> {
        self.cfg.current()
    }

    fn mark_contact(&self) {
        self.last_contact.store(now_secs(), Ordering::Relaxed);
    }

    /// Time since the last successful server contact.
    fn since_contact(&self) -> Duration {
        Duration::from_secs(now_secs().saturating_sub(self.last_contact.load(Ordering::Relaxed)))
    }

    /// Run forever: heartbeat in the background, claim + build in the foreground.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        // One heartbeat before the first claim, so a worker that has just
        // started runs its server-set values from its first build rather than
        // from one heartbeat later. Not over registration, which runs before
        // the worker has its certificate and answers anyone who knows a
        // fingerprint; the heartbeat is authenticated.
        self.beat().await;

        let hb = Arc::clone(&self);
        tokio::spawn(async move { hb.heartbeat_loop().await });

        let cfg = self.cfg();
        tracing::info!(
            "Worker '{}' online: arches={:?} concurrency={} executor={}",
            cfg.name,
            cfg.arches,
            cfg.tunables.concurrency,
            self.executor.describe_self()
        );

        loop {
            // A place before claiming so we never hold a job we can't run.
            let permit = self.gate.acquire().await;
            // After the wait, not before: the wait can outlast a delivery.
            let cfg = self.cfg();

            // Ask before claiming rather than after: a job we cannot start yet
            // is better left queued on the server, where it is visible and
            // costs nothing, than held here against a lease.
            let claimed = self.active.lock().await.len();
            if !self.executor.ready_for_work(claimed).await {
                drop(permit);
                tokio::time::sleep(cfg.tunables.poll_interval).await;
                continue;
            }

            let claim = ClaimRequest {
                mirrorlist: self.mirrorlist_preference(&cfg).await,
            };
            match self.client.claim(&claim).await {
                Ok(Some(job)) => {
                    self.mark_contact();
                    let claimed = ClaimedJob {
                        mirrorlist: self.job_mirrorlist(&cfg, &job).await,
                        timeout: Timeout::for_job(cfg.tunables.build_timeout, &job),
                        cancel: CancellationToken::new(),
                        job,
                    };
                    // Counted here rather than in the task: the next
                    // readiness check may run before the task does, and
                    // must see this job.
                    self.active
                        .lock()
                        .await
                        .insert(claimed.job.build_id, claimed.cancel.clone());
                    let this = Arc::clone(&self);
                    tokio::spawn(async move {
                        this.run_one(claimed).await;
                        drop(permit);
                    });
                }
                Ok(None) => {
                    self.mark_contact();
                    drop(permit);
                    tokio::time::sleep(cfg.tunables.poll_interval).await;
                }
                Err(e) => {
                    tracing::warn!("claim failed: {e}");
                    drop(permit);
                    tokio::time::sleep(cfg.tunables.poll_interval).await;
                }
            }
        }
    }

    /// What to tell the server about mirrorlists on the next claim.
    async fn mirrorlist_preference(&self, cfg: &CoreConfig) -> MirrorlistPreference {
        if cfg.mirrorlist.is_some() {
            return MirrorlistPreference::Local;
        }
        MirrorlistPreference::Server {
            checksums: self
                .mirrorlists
                .lock()
                .await
                .iter()
                .map(|(arch, held)| (arch.clone(), held.checksum.clone()))
                .collect(),
        }
    }

    /// The mirrorlist `job` builds with; see [`resolve_mirrorlist`].
    ///
    /// Resolved here so the executors get the content itself, and none of them
    /// has to know that it may have arrived on an earlier job.
    async fn job_mirrorlist(&self, cfg: &CoreConfig, job: &JobDescriptor) -> Option<String> {
        let mut held = self.mirrorlists.lock().await;
        resolve_mirrorlist(
            cfg.mirrorlist.as_deref(),
            &mut held,
            &job.arch,
            &job.mirrorlist,
        )
    }

    /// Execute a single job with panic-safe, always-emitted completion.
    async fn run_one(self: &Arc<Self>, claimed: ClaimedJob) {
        let build_id = claimed.job.build_id;
        tracing::info!("Building {}", report::describe(&claimed.job));

        // Panic-wrap so a task panic still yields a terminal completion.
        let executor = Arc::clone(&self.executor);
        let client = Arc::clone(&self.client);
        let result = tokio::spawn(async move { executor.run_job(client, claimed).await }).await;

        let report = match result {
            Ok(Ok(report)) => report,
            Ok(Err(e)) => {
                let report = report::from_error(&e);
                let reason = report.reason.as_deref().unwrap_or_default();
                log(&self.client, build_id, &format!("\n[worker] {reason}\n")).await;
                report
            }
            Err(e) => report::interrupted(format!("worker task panicked: {e}")),
        };

        self.report_completion(build_id, &report).await;
        self.active.lock().await.remove(&build_id);
    }

    /// Send the terminal completion, retrying briefly so a transient network
    /// blip does not drop the report (which would cost the build to the lease
    /// reaper). Sending it again is safe: a report the server took, whose
    /// answer was lost, is accepted again as a repeat.
    async fn report_completion(&self, build_id: i32, report: &CompleteReport) {
        let mut attempt = 1;
        loop {
            let error = match self.client.complete(build_id, report).await {
                Ok(()) => {
                    self.mark_contact();
                    if report.outcome == BuildOutcome::Succeeded {
                        tracing::info!("Build {build_id} completed successfully");
                    } else {
                        tracing::warn!(
                            "Build {build_id} failed: {}",
                            report.reason.as_deref().unwrap_or("unknown")
                        );
                    }
                    return;
                }
                Err(error) => error,
            };
            // `{error:#}` for the cause chain, not just the outermost context:
            // the server's reason is what tells a lost lease from anything else
            // it refused.
            tracing::warn!("reporting completion for {build_id} failed: {error:#}");
            match after_failure(&error, attempt) {
                Next::Retry(delay) => tokio::time::sleep(delay).await,
                Next::Refused => return,
                Next::Exhausted => {
                    tracing::error!(
                        "gave up reporting completion for {build_id}; the server will abandon it when its lease expires"
                    );
                    return;
                }
            }
            attempt += 1;
        }
    }

    /// The lease timing of this worker's latest registration.
    fn lease(&self) -> Lease {
        self.client
            .lease()
            .expect("a runner's client is authenticated, and so built with a lease")
    }

    /// Background heartbeat + self-abort watchdog.
    async fn heartbeat_loop(self: Arc<Self>) {
        loop {
            let interval = self.lease().heartbeat_interval;
            tokio::time::sleep(interval).await;
            self.beat().await;
        }
    }

    /// One heartbeat: renew the leases, act on what the server answers, and
    /// abort everything if the server has been out of reach past the lease.
    async fn beat(&self) {
        let active_build_ids: Vec<i32> = {
            let guard = self.active.lock().await;
            guard.keys().copied().collect()
        };

        // Only when it differs from what the server holds: the first heartbeat
        // of a worker process, and the one after each delivery.
        let cfg = self.cfg();
        let effective = cfg.settings.effective();
        let report = {
            let reported = self.reported_config.lock().await;
            (reported.as_ref() != Some(&effective)).then(|| effective.clone())
        };

        let hb = Heartbeat {
            active_build_ids: active_build_ids.clone(),
            version: cfg.version.clone(),
            effective: report.clone(),
            received_revision: effective.received_revision.clone(),
        };
        match self.client.heartbeat(&hb).await {
            Ok(resp) => {
                self.mark_contact();
                // Only once the server has taken it: a heartbeat that never
                // arrived has told it nothing.
                if report.is_some() {
                    *self.reported_config.lock().await = Some(effective);
                }
                // The server's abort list: any of our builds that were
                // abandoned (lease lost) or cancelled by an operator. This is
                // the only way a build learns it.
                if !resp.cancel.is_empty() {
                    let guard = self.active.lock().await;
                    for id in &resp.cancel {
                        if let Some(cancel) = guard.get(id) {
                            cancel.cancel();
                            tracing::warn!("server asked to stop build #{id}; aborting");
                        }
                    }
                }
                if let Some(snapshot) = resp.config {
                    self.apply(&snapshot).await;
                }
                self.sync_registration().await;
            }
            Err(e) => tracing::warn!("heartbeat failed: {e}"),
        }

        // Self-abort: if the server has been unreachable past the lease TTL,
        // the builds we hold have already been abandoned — stop them to avoid
        // a zombie upload.
        let lease_ttl = self.lease().ttl;
        if !active_build_ids.is_empty() && self.since_contact() > lease_ttl {
            tracing::error!(
                "server unreachable for {}s (> lease {}s); self-aborting {} build(s)",
                self.since_contact().as_secs(),
                lease_ttl.as_secs(),
                active_build_ids.len()
            );
            let guard = self.active.lock().await;
            for cancel in guard.values() {
                cancel.cancel();
            }
        }
    }

    /// Take in values the server delivered.
    ///
    /// Resolved against the settings, then handed to the executor, which may
    /// refuse what the machine cannot honour; what it hands back is what runs.
    /// The concurrency gate follows before anything registers again, so the
    /// server never records more capacity than the worker has. Builds already
    /// running keep the configuration they started with.
    async fn apply(&self, snapshot: &ConfigSnapshot) {
        let current = self.cfg();
        let candidate = current.settings.with_snapshot(snapshot);
        let settings = self
            .executor
            .reconfigure(candidate, &current.settings)
            .await;
        let next = current.with_settings(settings);

        let changed = next
            .settings
            .effective()
            .changed_since(&current.settings.effective());
        if !changed.is_empty() {
            tracing::info!(
                "Server delivered settings (revision {}); changed: {}",
                snapshot.revision,
                changed.join(", ")
            );
        }
        self.gate.set_target(next.tunables.concurrency);
        self.cfg.replace(next);
    }

    /// Register again if what the worker schedules by has changed since the
    /// server last accepted a registration.
    ///
    /// Safe while builds run: registration is an upsert keyed on the
    /// certificate fingerprint that refreshes what the worker reports and
    /// touches no lease, build or approval.
    async fn sync_registration(&self) {
        let cfg = self.cfg();
        let routing = Routing::of(&cfg);
        let mut registered = self.registered.lock().await;
        if *registered == routing {
            return;
        }
        // The certificate request enrollment made: the server only signs it
        // if it holds no usable certificate.
        let Some(csr_pem) = self.client.registered_csr() else {
            tracing::error!("this worker's client holds no registration to send again");
            return;
        };
        let request = crate::enroll::register_request(&cfg, csr_pem, E::KIND);
        match self.client.register(&request).await {
            Ok(_) => {
                tracing::info!(
                    "Registered again with concurrency {}, priority {}, packages {:?}",
                    routing.concurrency,
                    routing.priority,
                    routing.packages
                );
                *registered = routing;
            }
            // Retried on the next heartbeat. The gate already holds the new
            // concurrency, so a server still holding the old one can only ever
            // offer this worker less work than it would take, never more.
            Err(e) => tracing::warn!("registering the new settings failed: {e:#}"),
        }
    }
}

#[cfg(test)]
mod completion_tests {
    use super::{COMPLETION_ATTEMPTS, Next, after_failure};
    use crate::client::Refused;
    use reqwest::StatusCode;
    use std::time::Duration;

    fn refused(status: StatusCode) -> anyhow::Error {
        anyhow::Error::new(Refused::new(
            "complete",
            status,
            "build 7 is no longer owned",
        ))
    }

    /// A lost lease or a vanished build is the server's verdict; asking again
    /// only holds the slot.
    #[test]
    fn a_refusal_is_not_retried() {
        assert_eq!(
            after_failure(&refused(StatusCode::FORBIDDEN), 1),
            Next::Refused
        );
        assert_eq!(
            after_failure(&refused(StatusCode::NOT_FOUND), 1),
            Next::Refused
        );
    }

    /// A server error or a request that got no answer may well go through
    /// next time -- and a report already taken is accepted again as a repeat.
    #[test]
    fn a_failure_the_server_did_not_decide_is_retried() {
        let unreachable = anyhow::anyhow!("connection refused").context("complete request");
        assert_eq!(
            after_failure(&unreachable, 1),
            Next::Retry(Duration::from_secs(2))
        );
        assert_eq!(
            after_failure(&refused(StatusCode::INTERNAL_SERVER_ERROR), 2),
            Next::Retry(Duration::from_secs(4))
        );
    }

    /// No sleep after the last attempt: nothing comes after it.
    #[test]
    fn the_last_attempt_ends_without_waiting() {
        let unreachable = anyhow::anyhow!("connection refused");
        assert_eq!(
            after_failure(&unreachable, COMPLETION_ATTEMPTS),
            Next::Exhausted
        );
    }
}

#[cfg(test)]
mod mirrorlist_tests {
    use aurcache_common::worker::MirrorlistOffer;
    use std::collections::BTreeMap;

    use super::HeldMirrorlist;
    use super::resolve_mirrorlist as resolve;

    fn send(content: &str, checksum: &str) -> MirrorlistOffer {
        MirrorlistOffer::Send {
            content: content.to_string(),
            checksum: checksum.to_string(),
        }
    }

    fn held_list(checksum: &str, content: &str) -> HeldMirrorlist {
        HeldMirrorlist {
            checksum: checksum.to_string(),
            content: content.to_string(),
        }
    }

    /// Content that arrives is used and remembered, so the next claim can
    /// advertise it and the server can skip resending.
    #[test]
    fn content_is_used_and_cached() {
        let mut held = BTreeMap::new();
        let used = resolve(None, &mut held, "x86_64", &send("Server = a\n", "abc"));
        assert_eq!(used.as_deref(), Some("Server = a\n"));
        assert_eq!(held["x86_64"], held_list("abc", "Server = a\n"));
    }

    /// Withheld content is filled in from the cache: this is what makes the
    /// build see a mirrorlist that arrived on an earlier job.
    #[test]
    fn keep_is_filled_from_the_cache() {
        let mut held = BTreeMap::new();
        held.insert("x86_64".to_string(), held_list("abc", "Server = a\n"));
        let used = resolve(None, &mut held, "x86_64", &MirrorlistOffer::Keep);
        assert_eq!(used.as_deref(), Some("Server = a\n"));
    }

    /// "The server has none" must clear the cache rather than reuse it, or a
    /// mirrorlist removed on the server would live on in every worker.
    #[test]
    fn no_server_mirrorlist_drops_the_cached_one() {
        let mut held = BTreeMap::new();
        held.insert("x86_64".to_string(), held_list("abc", "Server = a\n"));
        let used = resolve(None, &mut held, "x86_64", &MirrorlistOffer::Nothing);
        assert!(used.is_none());
        assert!(held.is_empty(), "a stale entry would outlive the server's");
    }

    /// A worker's own mirrorlist wins over anything the server sends.
    #[test]
    fn a_local_mirrorlist_wins() {
        let mut held = BTreeMap::new();
        let used = resolve(
            Some("Server = mine\n"),
            &mut held,
            "x86_64",
            &send("Server = server\n", "abc"),
        );
        assert_eq!(used.as_deref(), Some("Server = mine\n"));
    }

    /// Caches are per architecture: a worker building two of them must not
    /// hand one arch's mirrors to the other.
    #[test]
    fn caches_do_not_cross_architectures() {
        let mut held = BTreeMap::new();
        resolve(None, &mut held, "x86_64", &send("Server = x\n", "cx"));
        resolve(None, &mut held, "aarch64", &send("Server = a\n", "ca"));
        assert_eq!(held["x86_64"].content, "Server = x\n");
        assert_eq!(held["aarch64"].content, "Server = a\n");
    }
}
