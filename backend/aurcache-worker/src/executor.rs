//! The `devtools` chroot executor.
//!
//! Owns the state that spans concurrent builds on this worker ([`Shared`]):
//! among it the pkgbases with a live `SRCDEST`, which the cache
//! garbage-collector must not evict out from under a sibling build and which
//! no two builds may fetch into at once.

use anyhow::Result;
use aurcache_common::worker::CompleteReport;
use aurcache_common::worker_config::EffectiveSource;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::executor::{ClaimedJob, Executor};
use aurcache_worker_core::live::Live;
use aurcache_worker_core::settings::WorkerSettings;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::agent::NamedKeysGate;
use crate::build_firewall::{BuildNetwork, ServerHosts, Table};
use crate::cache::{Cache, CacheLimits};
use crate::cgroup::{BuildLimits, Hierarchy};
use crate::chroots::Chroots;
use crate::config::{Config, Tunables};
use crate::job;
use crate::repo_db::RepoDbs;
use crate::settings::keys;
use crate::srcdest_lock::SrcdestLocks;

/// Every limit key, per build and in total.
const LIMIT_KEYS: [&str; 6] = [
    keys::BUILD_MEMORY_MAX,
    keys::BUILD_SWAP_MAX,
    keys::BUILD_CPUS,
    keys::TOTAL_BUILD_MEMORY_MAX,
    keys::TOTAL_BUILD_SWAP_MAX,
    keys::TOTAL_BUILD_CPUS,
];
const CPU_KEYS: [&str; 2] = [keys::BUILD_CPUS, keys::TOTAL_BUILD_CPUS];
const TOTAL_KEYS: [&str; 3] = [
    keys::TOTAL_BUILD_MEMORY_MAX,
    keys::TOTAL_BUILD_SWAP_MAX,
    keys::TOTAL_BUILD_CPUS,
];

/// State that spans the concurrent builds on this worker.
///
/// Shared with every job rather than rebuilt per job, because each part has
/// to see all of them: the cache garbage-collector must know which `SRCDEST`
/// directories are live, the chroots must know which copies are, and what
/// was learnt of the package cache and the repository is worth keeping
/// from one build to the next.
pub struct Shared {
    /// Who holds which pkgbase's `SRCDEST`. Both the right to fetch into one
    /// and the record that stops the garbage-collector evicting it; see
    /// [`crate::srcdest_lock`].
    pub srcdest: Arc<SrcdestLocks>,
    /// Where a build gets its chroot, and gives it back.
    pub chroots: Chroots,
    /// The caches, with what has been verified of the package cache.
    cache: Cache,
    /// The repository databases fetched so far, for conditional fetches.
    pub repo_dbs: RepoDbs,
    /// The SSH keys this worker holds for builds, by name: the socket of the
    /// agent it started, as the default key, or none.
    pub ssh_keys: BTreeMap<String, PathBuf>,
    /// Asked before each build when some of those are named keys, which a
    /// build is offered only while it holds.
    pub named_keys_gate: Option<NamedKeysGate>,
}

impl Shared {
    /// Set up, with nothing open yet (see [`Chroots::open`]), holding
    /// `ssh_keys` for builds, the named ones behind `named_keys_gate`.
    #[must_use]
    pub fn new(
        cfg: &Config,
        ssh_keys: BTreeMap<String, PathBuf>,
        named_keys_gate: Option<NamedKeysGate>,
    ) -> Self {
        let tunables = &cfg.tunables;
        Self {
            srcdest: SrcdestLocks::new(),
            chroots: Chroots::new(
                cfg.pool_config(),
                tunables.chroot_refresh_interval,
                tunables.keep_failed,
                cfg.cache_owner(),
            ),
            cache: Cache::new(&cfg.cache_dir, tunables.cache),
            repo_dbs: RepoDbs::default(),
            ssh_keys,
            named_keys_gate,
        }
    }

    /// Whether a build may be offered a named key now: always, unless the
    /// gate has closed, the sandbox, asked again, no longer keeping builds
    /// from the agents or not answering.
    pub async fn named_keys_held(&self) -> bool {
        match &self.named_keys_gate {
            None => true,
            Some(gate) => gate.holds().await,
        }
    }

    /// The caches with `limits` in force, made in the pool once it is open.
    pub async fn cache(&self, limits: CacheLimits) -> Cache {
        self.cache
            .clone()
            .with_limits(limits)
            .with_volumes(self.chroots.cache_volumes().await)
    }
}

/// Builds each package in its own `devtools` chroot copy.
pub struct ChrootExecutor {
    /// The configuration in force. Each job takes the one current when it
    /// starts and keeps it: a build keeps the limits it started with, because
    /// a build sized for one limit and killed by another is worse than
    /// waiting for the next build.
    cfg: Live<Config>,
    shared: Arc<Shared>,
    /// The prepared cgroup subtree each build's cgroup is created under, so
    /// `memory.peak` reports one build rather than the worker and its siblings.
    ///
    /// `None` where the subtree could not be prepared. Then the network policy
    /// cannot be put in place, and the worker takes no builds unless
    /// `WORKER_BUILD_FIREWALL=0` turns it off; those builds report no memory
    /// figure, which the page shows as unknown.
    cgroups: Option<Hierarchy>,
    /// What builds may connect to, matched on their cgroups under
    /// [`Self::cgroups`].
    network: BuildNetwork,
    /// When to say again why this worker takes no builds.
    declining: Reminder,
}

/// How often a worker that takes no builds says why, beyond its startup
/// log: it stays online, and only its log tells why builds go elsewhere.
const DECLINING_EVERY: Duration = Duration::from_secs(600);

/// Something said once, to say again at most once per `every`.
struct Reminder {
    every: Duration,
    last: Mutex<Instant>,
}

impl Reminder {
    /// One last said at `said`.
    const fn new(every: Duration, said: Instant) -> Self {
        Self {
            every,
            last: Mutex::new(said),
        }
    }

    /// Whether it is time to say it again, as of `now`. When it is, the next
    /// time counts from `now`.
    fn due(&self, now: Instant) -> bool {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.saturating_duration_since(*last) < self.every {
            return false;
        }
        *last = now;
        true
    }
}

impl ChrootExecutor {
    /// Async because it settles how chroots are made before any job arrives:
    /// the answer needs a real mount to be sure of, and finding out per build
    /// would mean a warning per build on a filesystem that cannot do it.
    ///
    /// `worker_id`, the identity's fingerprint, names its nftables table;
    /// `server` names the hosts that builds are refused, except for the
    /// repository.
    ///
    /// Builds are offered only `ssh_keys`, never an agent this process's
    /// environment names, and the named ones only while `named_keys_gate`
    /// holds.
    pub async fn new(
        cfg: Config,
        worker_id: &str,
        server: ServerHosts,
        ssh_keys: BTreeMap<String, PathBuf>,
        named_keys_gate: Option<NamedKeysGate>,
    ) -> Self {
        let cgroups = prepare_cgroups(&cfg.tunables);
        log_limits(&cfg.tunables, cgroups.is_some());
        // After the hierarchy, whose `builds/` the rules match, and before any
        // job can be claimed.
        let network = BuildNetwork::install(Table::for_worker(worker_id), cgroups.as_ref(), server);
        let shared = Arc::new(Shared::new(&cfg, ssh_keys, named_keys_gate));
        // Before anything can claim work: no build of ours is running yet, so
        // whatever a build left in the pool belongs to a run that is over.
        if shared.chroots.open().await.is_ok() {
            let cleared = shared
                .chroots
                .sweep(&std::collections::HashSet::new())
                .await;
            if cleared > 0 {
                tracing::info!("removed {cleared} build(s) a previous run left in the pool");
            }
        }
        Self {
            cfg: Live::new(cfg),
            shared,
            cgroups,
            network,
            // The policy's startup log says it first.
            declining: Reminder::new(DECLINING_EVERY, Instant::now()),
        }
    }

    /// The limits in `settings` this machine cannot enforce, with why.
    ///
    /// Only the server's values are refused. A limit pinned in the machine's
    /// own environment that cannot be enforced keeps its startup behaviour --
    /// builds are refused rather than run without it -- because that is the
    /// operator of that machine saying the limit matters more than building.
    /// A value from the server that could not be enforced would instead fail
    /// every build until someone noticed, so it is refused and the previous
    /// value stands.
    fn unenforceable(
        &self,
        settings: &WorkerSettings,
        next: &Tunables,
    ) -> BTreeMap<String, String> {
        let Some(hierarchy) = &self.cgroups else {
            return refusals(
                settings,
                &LIMIT_KEYS,
                "this worker has no cgroup to enforce build limits in (see its startup log)",
            );
        };
        if (next.build_limits.cpus.is_some() || next.total_build_limits.cpus.is_some())
            && let Err(e) = hierarchy.enable_cpu()
        {
            return refusals(
                settings,
                &CPU_KEYS,
                &format!("the cpu controller cannot be enabled on this worker: {e:#}"),
            );
        }
        BTreeMap::new()
    }
}

/// The server's values among `keys`, refused for `why`.
///
/// Only a value that sets a limit: one that removes a limit asks nothing of
/// the machine, so there is nothing about it to refuse.
fn refusals(settings: &WorkerSettings, keys: &[&str], why: &str) -> BTreeMap<String, String> {
    keys.iter()
        .filter(|key| {
            settings.source(key) == Some(EffectiveSource::Server) && settings.raw(key).is_some()
        })
        .map(|key| ((*key).to_string(), why.to_string()))
        .collect()
}

/// The cgroup subtree builds run under, prepared once, at startup: the
/// hierarchy has to be rearranged before any build runs, and rearranging it
/// per build would move the worker repeatedly.
///
/// `None` where it cannot be prepared, or cannot take the limits `tunables`
/// configures -- and then a build with limits configured is refused rather
/// than run without them.
fn prepare_cgroups(tunables: &Tunables) -> Option<Hierarchy> {
    let hierarchy = match Hierarchy::prepare() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                "No per-build cgroup ({e:#}), so the build network policy cannot \
                 be put in place and this worker takes no builds (with \
                 WORKER_BUILD_FIREWALL=0, builds run but report no peak memory). \
                 A container needs `privileged`; a native install needs \
                 `Delegate=yes` on the unit."
            );
            return None;
        }
    };
    let (limits, total) = (&tunables.build_limits, &tunables.total_build_limits);
    // Before the total: its `cpu.max` on `builds/` needs the controller.
    // A failure here refuses builds on its own, when a build's cgroup
    // cannot take its `cpu.max` either.
    if (limits.cpus.is_some() || total.cpus.is_some())
        && let Err(e) = hierarchy.enable_cpu()
    {
        tracing::error!(
            "a CPU limit is set, but the cpu controller cannot be enabled ({e:#}); \
             every build will be refused rather than run unlimited"
        );
    }
    // Always applied, set or not: `builds/` outlives the worker, and a
    // total removed from the configuration has to be taken off it
    // rather than left from the previous run.
    match hierarchy.apply_total(total) {
        Ok(()) => Some(hierarchy),
        Err(e) if total.is_empty() => {
            tracing::warn!("could not clear total build limits ({e:#})");
            Some(hierarchy)
        }
        // Without the hierarchy `run_build` refuses every build, which is
        // the point: a total that is configured is enforced or nothing
        // runs.
        Err(e) => {
            tracing::error!(
                "WORKER_TOTAL_BUILD_* limits are set but cannot be applied ({e:#}); \
                 every build will be refused rather than run without them"
            );
            None
        }
    }
}

/// Say, once at startup, what limits builds run under -- or that they will be
/// refused, rather than only have every build fail.
fn log_limits(tunables: &Tunables, have_cgroups: bool) {
    let (limits, total) = (&tunables.build_limits, &tunables.total_build_limits);
    if limits.is_empty() && total.is_empty() {
        return;
    }
    if !have_cgroups {
        tracing::error!(
            "build resource limits (WORKER_BUILD_* or WORKER_TOTAL_BUILD_*) are set, \
             but there is no cgroup to enforce them in; every build will be refused \
             rather than run unlimited"
        );
        return;
    }
    let describe = |l: &BuildLimits| {
        let gib =
            |bytes: Option<u64>| bytes.map_or_else(|| "unlimited".to_string(), crate::chroots::gib);
        format!(
            "memory {}, swap {}, CPUs {}",
            gib(l.memory_max),
            gib(l.swap_max),
            l.cpus
                .map_or_else(|| "unlimited".to_string(), |c| c.to_string()),
        )
    };
    tracing::info!(
        "Build limits: each build {}; all builds together {}",
        describe(limits),
        describe(total),
    );
}

impl Executor for ChrootExecutor {
    const KIND: &'static str = "chroot";

    async fn run_job(
        &self,
        client: Arc<WorkerClient>,
        claimed: ClaimedJob,
    ) -> Result<CompleteReport> {
        // Taken before building, and held until the report is on its way: it
        // is what protects this job's own SRCDEST (and every sibling's) from
        // the cache GC that runs at each job's start, and what keeps a second
        // build of the same pkgbase -- another platform of it -- from fetching
        // into the same directory meanwhile. Anything that reads what this
        // build used belongs inside this scope, while the sources still say
        // what they said.
        let _srcdest = self.shared.srcdest.acquire(&claimed.job.pkgbase).await;

        // Once, here: everything this build does reads this copy.
        let cfg = self.cfg.current();
        let machine = job::Machine {
            cgroups: self.cgroups.as_ref(),
            network: &self.network,
        };
        job::run_job(&cfg, machine, &client, &claimed, &self.shared).await
    }

    async fn ready_for_work(&self, claimed: usize) -> bool {
        // Every build would be refused: claiming one only fails it, and keeps
        // it from a worker that could build it.
        if let Some(reason) = self.network.unavailable_reason() {
            if self.declining.due(Instant::now()) {
                tracing::error!("not taking builds: {reason}");
            }
            return false;
        }
        let build_limit = self.cfg.current().tunables.build_disk_max;
        self.shared
            .chroots
            .ready_for_work(build_limit, claimed)
            .await
    }

    fn describe_self(&self) -> String {
        format!(
            "devtools chroot ({})",
            self.cfg.current().chroot_dir.display()
        )
    }

    /// Take delivered values: refuse the limits this machine cannot enforce,
    /// put new totals on the builds' cgroup at once, and keep the rest for the
    /// next build.
    async fn reconfigure(
        &self,
        settings: WorkerSettings,
        previous: &WorkerSettings,
    ) -> WorkerSettings {
        let current = self.cfg.current();
        let mut settings = settings;
        let mut next = current.with_settings(&settings);

        let refused = self.unenforceable(&settings, &next.tunables);
        if !refused.is_empty() {
            tracing::warn!("refusing build limits from the server: {refused:?}");
            settings = settings.refused(&refused, previous);
            next = current.with_settings(&settings);
        }

        // Totals bound what the builds already running use between them, so
        // they go on now rather than with the next build -- including a lower
        // one, which is what an operator asked for even when the running
        // builds are over it.
        let total = &next.tunables.total_build_limits;
        if *total != current.tunables.total_build_limits
            && let Some(hierarchy) = &self.cgroups
            && let Err(e) = hierarchy.apply_total(total)
        {
            tracing::warn!("could not apply the delivered total build limits ({e:#})");
            let why = format!("the total could not be applied on this worker: {e:#}");
            let refused = refusals(&settings, &TOTAL_KEYS, &why);
            settings = settings.refused(&refused, previous);
            next = current.with_settings(&settings);
            // Best-effort: a partial write must not leave a mix of the two.
            let _ = hierarchy.apply_total(&next.tunables.total_build_limits);
        }

        let tunables = &next.tunables;
        // Like the memory totals, the disk total bounds the builds already
        // running, so it goes on now.
        if tunables.disk_max != current.tunables.disk_max
            && let Err(e) = self.shared.chroots.set_total(tunables.disk_max).await
        {
            tracing::warn!("could not apply the delivered WORKER_DISK_MAX ({e:#})");
        }
        self.shared
            .chroots
            .set_interval(tunables.chroot_refresh_interval);
        self.shared.chroots.set_keep_failed(tunables.keep_failed);
        self.cfg.replace(next);
        settings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reminder_is_due_once_per_interval_after_it_was_said() {
        let start = Instant::now();
        let reminder = Reminder::new(Duration::from_secs(600), start);
        assert!(!reminder.due(start));
        assert!(!reminder.due(start + Duration::from_secs(599)));
        assert!(reminder.due(start + Duration::from_secs(600)));
        assert!(!reminder.due(start + Duration::from_secs(601)));
        assert!(reminder.due(start + Duration::from_secs(1200)));
    }

    #[test]
    fn only_a_policy_that_could_not_be_put_in_place_declines_builds() {
        let unavailable = BuildNetwork::Unavailable("no nft".to_string());
        assert_eq!(unavailable.unavailable_reason(), Some("no nft"));
        assert_eq!(BuildNetwork::Unrestricted.unavailable_reason(), None);
    }
}
