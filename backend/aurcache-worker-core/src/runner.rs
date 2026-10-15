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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::client::WorkerClient;
use crate::config::CoreConfig;
use crate::executor::Executor;
use crate::gate::ConcurrencyGate;
use crate::report;

/// What a worker needs to register again while it runs.
///
/// Registration carries its concurrency, priority and package affinity, which
/// the server's scheduler reads from the worker's row. When delivered values
/// change any of them, the worker registers again with what it now runs, so
/// neither end has to know that the `concurrency` setting and the registered
/// concurrency are the same thing.
pub struct Registration {
    /// A certificate request for the worker's own key, as enrollment made
    /// one. The server only signs it if it holds no usable certificate.
    pub csr_pem: String,
    /// The executor's kind, as [`Executor::KIND`] names it.
    pub kind: &'static str,
}

/// The fields of a registration the server schedules by.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Routing {
    packages: Vec<String>,
    priority: i32,
    concurrency: usize,
}

impl Routing {
    fn of(cfg: &CoreConfig) -> Self {
        Self {
            packages: cfg.packages.clone(),
            priority: cfg.priority,
            concurrency: cfg.concurrency,
        }
    }
}

/// Shared runtime state for the worker loop.
pub struct Runner<E: Executor> {
    /// The configuration in force, replaced whole when the server delivers
    /// values. Read through [`Self::cfg`], which hands out the current one: a
    /// loop iteration or a job works with one copy from start to end.
    cfg: RwLock<Arc<CoreConfig>>,
    client: Arc<WorkerClient>,
    executor: Arc<E>,
    registration: Registration,
    /// Build ids claimed and not yet reported (reported in each heartbeat).
    active: Mutex<HashMap<i32, Arc<AtomicBool>>>,
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
    /// what the server is taken to hold until this registers again.
    pub fn new(
        cfg: Arc<CoreConfig>,
        client: Arc<WorkerClient>,
        executor: Arc<E>,
        registration: Registration,
    ) -> Arc<Self> {
        Arc::new(Self {
            gate: ConcurrencyGate::new(cfg.concurrency),
            registered: Mutex::new(Routing::of(&cfg)),
            cfg: RwLock::new(cfg),
            client,
            executor,
            registration,
            active: Mutex::new(HashMap::new()),
            last_contact: AtomicU64::new(now_secs()),
            mirrorlists: Mutex::new(BTreeMap::new()),
            reported_config: Mutex::new(None),
        })
    }

    /// The configuration in force now.
    fn cfg(&self) -> Arc<CoreConfig> {
        // Poisoned only by a panic mid-assignment of an `Arc`, which leaves
        // either the old value or the new one -- both usable.
        Arc::clone(
            &self
                .cfg
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn mark_contact(&self) {
        self.last_contact.store(now_secs(), Ordering::Relaxed);
    }

    /// Seconds since the last successful server contact.
    fn since_contact(&self) -> u64 {
        now_secs().saturating_sub(self.last_contact.load(Ordering::Relaxed))
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
            "Worker '{}' online: native={:?} emulated={:?} concurrency={} executor={}",
            cfg.name,
            cfg.native_arches,
            cfg.emulated_arches,
            cfg.concurrency,
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
                tokio::time::sleep(Duration::from_secs(cfg.poll_interval)).await;
                continue;
            }

            let claim = ClaimRequest {
                mirrorlist: self.mirrorlist_preference(&cfg).await,
            };
            match self.client.claim(&claim).await {
                Ok(Some(job)) => {
                    self.mark_contact();
                    let mirrorlist = self.job_mirrorlist(&cfg, &job).await;
                    // Counted here rather than in the task: the next
                    // readiness check may run before the task does, and
                    // must see this job.
                    let cancel = Arc::new(AtomicBool::new(false));
                    self.active
                        .lock()
                        .await
                        .insert(job.build_id, Arc::clone(&cancel));
                    let this = Arc::clone(&self);
                    tokio::spawn(async move {
                        this.run_one(job, mirrorlist, cancel).await;
                        drop(permit);
                    });
                }
                Ok(None) => {
                    self.mark_contact();
                    drop(permit);
                    tokio::time::sleep(Duration::from_secs(cfg.poll_interval)).await;
                }
                Err(e) => {
                    tracing::warn!("claim failed: {e}");
                    drop(permit);
                    tokio::time::sleep(Duration::from_secs(cfg.poll_interval)).await;
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
    async fn run_one(
        self: &Arc<Self>,
        job: JobDescriptor,
        mirrorlist: Option<String>,
        cancel: Arc<AtomicBool>,
    ) {
        let build_id = job.build_id;
        tracing::info!("Building {}", report::describe(&job));

        // Panic-wrap so a task panic still yields a terminal completion.
        let executor = Arc::clone(&self.executor);
        let client = Arc::clone(&self.client);
        let cancel_for_build = Arc::clone(&cancel);
        let result = tokio::spawn(async move {
            executor
                .run_job(client, job, mirrorlist, cancel_for_build)
                .await
        })
        .await;

        let report = match result {
            Ok(report) => report,
            Err(e) => report::interrupted(format!("worker task panicked: {e}")),
        };

        self.report_completion(build_id, &report).await;
        self.active.lock().await.remove(&build_id);
    }

    /// Send the terminal completion, retrying briefly so a transient network
    /// blip does not drop the report (which would cost the build to the lease
    /// reaper).
    async fn report_completion(&self, build_id: i32, report: &CompleteReport) {
        for attempt in 0..5 {
            match self.client.complete(build_id, report).await {
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
                Err(e) => {
                    // `{e:#}` for the cause chain, not just the outermost
                    // context: the server's reason is what tells a lost lease
                    // from anything else it refused.
                    tracing::warn!("reporting completion for {build_id} failed: {e:#}");
                    tokio::time::sleep(Duration::from_secs(2 * (attempt + 1))).await;
                }
            }
        }
        tracing::error!(
            "gave up reporting completion for {build_id}; the server will abandon it when its lease expires"
        );
    }

    /// Background heartbeat + self-abort watchdog.
    async fn heartbeat_loop(self: Arc<Self>) {
        loop {
            let interval = Duration::from_secs(self.cfg().heartbeat_interval.max(1));
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
            version: aurcache_common::version::full_version(env!("CARGO_PKG_VERSION")),
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
                // abandoned (lease lost) or cancelled by an operator. Set
                // each flag; the build's next 5 s tick then aborts it.
                if !resp.cancel.is_empty() {
                    let guard = self.active.lock().await;
                    for id in &resp.cancel {
                        if let Some(flag) = guard.get(id) {
                            flag.store(true, Ordering::SeqCst);
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
        if !active_build_ids.is_empty() && self.since_contact() > cfg.lease_ttl {
            tracing::error!(
                "server unreachable for {}s (> lease {}s); self-aborting {} build(s)",
                self.since_contact(),
                cfg.lease_ttl,
                active_build_ids.len()
            );
            let guard = self.active.lock().await;
            for flag in guard.values() {
                flag.store(true, Ordering::SeqCst);
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
        let settings = self.executor.reconfigure(candidate).await;
        let next = Arc::new(current.with_settings(settings));

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
        self.gate.set_target(next.concurrency);
        *self
            .cfg
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
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
        let request = crate::enroll::register_request(
            &cfg,
            self.registration.csr_pem.clone(),
            self.registration.kind,
        );
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
