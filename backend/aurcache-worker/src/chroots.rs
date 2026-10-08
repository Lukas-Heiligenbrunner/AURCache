//! Where a build gets its chroot, and how it gives it back.
//!
//! Every chroot lives in the worker's storage pool (`aurcache-chroot`): a
//! btrfs filesystem with simple quotas, backed by an image, a device or a
//! dedicated mount. The base chroot is a subvolume there, and a build's chroot
//! is a snapshot of it, taken by the worker before `makechrootpkg` runs, with
//! a second subvolume beside it for the build's workdir. Both sit in one quota
//! group limited to `WORKER_BUILD_DISK_MAX`, under the pool's total
//! (`WORKER_DISK_MAX`), so a build that sets out to fill the disk fails its own
//! build instead. See `design/implemented/build-disk-quota.md`.
//!
//! A snapshot is point-in-time, so the base is refreshed as a new snapshot,
//! swapped in: builds snapshot whatever `root` is at the instant they ask,
//! before or after the swap, and a refresh in flight never holds one up. The
//! overlay chroots, update layers and flattening this replaced existed because
//! an overlay's lower layer must not change for as long as a build reads it.

use crate::chroot;
use anyhow::{Context, Result, bail};
use aurcache_chroot::{BuildVolumes, Pool, PoolConfig, Usage};
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::protocol::report_warning;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

/// How long after a failed attempt to open the pool the next one is made.
/// Opening runs a handful of commands; asking before every claim would repeat
/// them every few seconds for as long as the host stays broken.
const REOPEN_AFTER: Duration = Duration::from_secs(60);

/// The build id a local one-shot build (`build-once`) uses in the pool. A
/// served build never has it, so the two cannot collide even when a one-shot
/// build shares a pool with a running worker.
pub const ONE_SHOT_BUILD_ID: i32 = i32::MAX;

/// The chroots on this worker: one base, and a snapshot per build.
pub struct Chroots {
    config: std::sync::Mutex<PoolConfig>,
    /// Owner of the cache subvolume made when the pool opens.
    cache_owner: aurcache_chroot::Owner,
    pool: RwLock<Option<Pool>>,
    /// When opening the pool last failed, and why, so retries are spaced out
    /// and a caller in between still hears the reason.
    last_failure: std::sync::Mutex<Option<OpenFailure>>,
    /// When the base chroot was last brought up to date, and the lock that
    /// serialises doing so. One lock for both, because "is it due" and "make
    /// it current" have to be one decision or two builds starting together
    /// both find it due.
    last_refresh: Mutex<Option<Instant>>,
    /// How long a refresh counts as current, in seconds. Atomic because it is
    /// a setting the server may change while builds run.
    interval: AtomicU64,
    /// How long a failed build's chroot is kept for inspection, `None` to
    /// keep nothing (`WORKER_KEEP_FAILED`). Read at sweep time, so lowering
    /// it shortens keeps already made.
    keep_failed: std::sync::Mutex<Option<Duration>>,
    /// Whether the last readiness check found the host short of room, so the
    /// change is logged once rather than at every poll.
    short_of_room: AtomicBool,
    /// Builds holding a lease now: a pool is only made again with none.
    active: Arc<AtomicUsize>,
    /// Whether the worker is draining to make an oversized pool again, so
    /// that is logged once.
    draining: AtomicBool,
}

impl Chroots {
    /// Chroots in the pool `config` describes. Nothing is mounted until
    /// [`Self::open`], so a pool that cannot be opened yet costs a worker its
    /// builds, not its ability to start and say why.
    #[must_use]
    pub fn new(
        config: PoolConfig,
        interval: Duration,
        keep_failed: Option<Duration>,
        cache_owner: aurcache_chroot::Owner,
    ) -> Self {
        Self {
            config: std::sync::Mutex::new(config),
            cache_owner,
            pool: RwLock::new(None),
            last_failure: std::sync::Mutex::new(None),
            last_refresh: Mutex::new(None),
            interval: AtomicU64::new(interval.as_secs()),
            keep_failed: std::sync::Mutex::new(keep_failed),
            short_of_room: AtomicBool::new(false),
            active: Arc::new(AtomicUsize::new(0)),
            draining: AtomicBool::new(false),
        }
    }

    /// Open the pool if it is not open yet.
    ///
    /// A failure is logged and retried no sooner than [`REOPEN_AFTER`]: a host
    /// without btrfs support or a free loop device does not start having one
    /// between two claims. Asked again within that window, this answers with
    /// the last failure rather than trying again.
    pub async fn open(&self) -> Result<()> {
        if self.pool.read().await.is_some() {
            return Ok(());
        }
        let mut pool = self.pool.write().await;
        if pool.is_some() {
            return Ok(());
        }
        {
            let last = self.last_failure.lock().expect("not poisoned");
            if let Some(failure) = last.as_ref()
                && failure.at.elapsed() < REOPEN_AFTER
            {
                bail!(
                    "the storage pool could not be opened ({}); trying again within {}s",
                    failure.error,
                    REOPEN_AFTER.saturating_sub(failure.at.elapsed()).as_secs()
                );
            }
        }
        let config = self.config.lock().expect("not poisoned").clone();
        let opened = async {
            let pool = Pool::open(config).await?;
            // The caches outlive every build, and a build writes into two of
            // them (its sources and its kept tree), so they are in the pool
            // too, under its total. Mode as the package's tmpfiles made the
            // cache directory: group-writable, the group inherited.
            pool.ensure_subvolume(crate::config::CACHE_SUBVOLUME, self.cache_owner, 0o2775)
                .await?;
            anyhow::Ok(pool)
        };
        match opened.await {
            Ok(opened) => {
                tracing::info!(
                    "storage pool mounted at {}{}",
                    opened.path().display(),
                    opened.total_usage().map_or_else(String::new, |u| format!(
                        ", {} of {} used",
                        gib(u.used),
                        u.limit.map_or_else(|| "unlimited".to_string(), gib)
                    ))
                );
                *pool = Some(opened);
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                    "the storage pool could not be opened ({e:#}); this worker takes no \
                     builds until it can, because a build without its disk quota could \
                     fill the host"
                );
                self.note_failure(&e);
                Err(e.context("opening the storage pool"))
            }
        }
    }

    /// Remember a failure to open (or make again) the pool, for
    /// [`Self::open`] to space its retries by.
    fn note_failure(&self, error: &anyhow::Error) {
        *self.last_failure.lock().expect("not poisoned") = Some(OpenFailure {
            at: Instant::now(),
            error: format!("{error:#}"),
        });
    }

    /// Whether a build limited to `build_limit` can start: once the pool is
    /// open, and -- for a sparse image -- while the host has room for the
    /// build to use its whole quota. `claimed` is how many jobs the worker
    /// holds, leased or not yet; see [`Self::shrink_when_idle`].
    ///
    /// Asked before claiming, not after: a claimed job holds a lease the
    /// server expects progress on, while one left queued waits where everyone
    /// can see it. The check is a guard, not a guarantee -- the host's space is
    /// shared with whatever else runs there -- and a host that fills up anyway
    /// fails the builds writing at that moment, not the pool.
    pub async fn ready_for_work(&self, build_limit: u64, claimed: usize) -> bool {
        // Both log their own failures, once each.
        if self.open().await.is_err() {
            return false;
        }
        match self.shrink_when_idle(claimed).await {
            Ok(PoolSize::Fits) => {}
            Ok(PoolSize::Draining) => return false,
            Err(e) => {
                tracing::error!("{e:#}");
                return false;
            }
        }
        let free = self.pool.read().await.as_ref().and_then(Pool::host_free);
        let short = free.is_some_and(|free| free < build_limit);
        let was_short = self.short_of_room.swap(short, Ordering::Relaxed);
        if short && !was_short {
            // Error, not warning: a worker that stops claiming is otherwise
            // only visible as builds going elsewhere, or nowhere.
            tracing::error!(
                "not taking builds: the host has {} free for the storage pool's image, less than \
                 one build's quota of {} (WORKER_BUILD_DISK_MAX). Free space there, or reserve \
                 the image with WORKER_DISK_RESERVE",
                gib(free.unwrap_or(0)),
                gib(build_limit)
            );
        } else if !short && was_short {
            tracing::info!("the host has room for a build again; taking builds");
        }
        !short
    }

    /// Change how long a refresh counts as current, from the next decision on.
    pub fn set_interval(&self, interval: Duration) {
        self.interval.store(interval.as_secs(), Ordering::Relaxed);
    }

    /// Change how long a failed build's chroot is kept, from the next sweep
    /// on -- including keeps already made, whose expiry is read from this.
    pub fn set_keep_failed(&self, keep_failed: Option<Duration>) {
        *self.keep_failed.lock().expect("not poisoned") = keep_failed;
    }

    /// How long a failed build's chroot is kept now, `None` to keep nothing.
    fn keep_failed(&self) -> Option<Duration> {
        *self.keep_failed.lock().expect("not poisoned")
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(self.interval.load(Ordering::Relaxed))
    }

    /// Set the total everything in the pool may use. Applies at once, to
    /// builds already running.
    ///
    /// An image that holds too much to shrink to it is made again, empty, at
    /// the new size -- once the builds running now have finished, which is
    /// why the worker takes no new ones meanwhile. See
    /// [`Self::shrink_when_idle`].
    pub async fn set_total(&self, total: u64) -> Result<()> {
        self.config.lock().expect("not poisoned").total = total;
        match self.pool.read().await.as_ref() {
            Some(pool) => pool.set_total(total).await.map(drop),
            // Opened with it, whenever it is.
            None => Ok(()),
        }
    }

    /// Whether the pool is the size its total asks for, making it again if it
    /// is not and nothing is building. An error is a failure to make it again.
    ///
    /// An image holding more than fits in its new size cannot shrink online,
    /// and the only way down is to delete it and make a new one. That throws
    /// the caches and the base chroot away -- they are rebuilt on the next
    /// build -- but not the worker's identity, which lives outside the pool.
    /// A build in flight holds a subvolume in it, so this waits for none to
    /// be, and the worker claims nothing new until then.
    ///
    /// In flight from the claim, not from the lease: a job claimed and still
    /// setting up -- its caches, a base refresh, its binds -- has no lease
    /// yet, but would find the pool gone under it, and its cache writing
    /// onto the host beneath the unmounted mountpoint. `claimed` counts
    /// those; no claim can happen meanwhile, since the same loop that asks
    /// this is the one that claims.
    async fn shrink_when_idle(&self, claimed: usize) -> Result<PoolSize> {
        let oversized = self.pool.read().await.as_ref().and_then(Pool::oversized);
        let Some(wanted) = oversized else {
            self.draining.store(false, Ordering::Relaxed);
            return Ok(PoolSize::Fits);
        };
        let active = claimed.max(self.active.load(Ordering::SeqCst));
        if active > 0 {
            if !self.draining.swap(true, Ordering::Relaxed) {
                tracing::error!(
                    "the storage pool holds more than fits in {} (WORKER_DISK_MAX was lowered): \
                     taking no builds until the {active} running finish, then making it again \
                     at that size, with cold caches",
                    gib(wanted)
                );
            }
            return Ok(PoolSize::Draining);
        }
        let mut guard = self.pool.write().await;
        // A lease may have been taken between the check and the lock, or
        // another caller made the pool again meanwhile.
        if self.active.load(Ordering::SeqCst) > 0 {
            return Ok(PoolSize::Draining);
        }
        let Some(pool) = guard.take() else {
            return Ok(PoolSize::Draining);
        };
        tracing::error!(
            "making the storage pool again at {}: its caches and base chroot start over",
            gib(wanted)
        );
        if let Err(e) = pool.destroy().await {
            self.note_failure(&e);
            return Err(e.context("removing the oversized pool; retrying later"));
        }
        *self.last_refresh.lock().await = None;
        self.draining.store(false, Ordering::Relaxed);
        drop(guard);
        // Made again the way a first start makes it.
        self.open().await?;
        Ok(PoolSize::Fits)
    }

    /// The cache subvolume's entries, for [`crate::cache::Cache`] to make each
    /// package's sources and kept tree a subvolume. `None` while the pool is
    /// not open.
    pub async fn cache_volumes(&self) -> Option<aurcache_chroot::CacheVolumes> {
        self.pool
            .read()
            .await
            .as_ref()
            .map(|pool| pool.cache_volumes(crate::config::CACHE_SUBVOLUME))
    }

    /// Remove what builds left in the pool, except those in `keep`: crash
    /// leftovers, kept failures past their time, and an interrupted refresh.
    /// Returns how many builds and kept failures were cleared. Nothing to do
    /// on a pool not open yet.
    pub async fn sweep(&self, keep: &HashSet<i32>) -> usize {
        match self.pool.read().await.as_ref() {
            Some(pool) => pool.sweep(keep, self.keep_failed()).await,
            None => 0,
        }
    }

    /// Bring the base chroot up to date, making it if it does not exist.
    ///
    /// Refreshing costs ~13s, and Arch's repositories move a few times a day,
    /// so it happens at most once per `WORKER_CHROOT_REFRESH_INTERVAL` rather
    /// than per build. Every build still syncs its own snapshot before it
    /// starts (`makechrootpkg -u`); this bounds how much that has to fetch.
    ///
    /// As a new snapshot, swapped in: the base is snapshotted to `root.next`,
    /// which is upgraded and checked, then exchanged into `root`'s place while
    /// the old base becomes `root.prev`. A build snapshots whatever `root` is
    /// at the instant it asks, before or after the swap, and is never exposed
    /// to a base halfway through an upgrade -- so a refresh in flight never
    /// holds up a build start, and a failed refresh leaves the base exactly as
    /// it was. See `design/implemented/btrfs-snapshots.md`.
    ///
    /// `report_to` names who to tell about a problem preparing the chroot, and
    /// the build to file it under -- `None` for `build-once`.
    pub async fn refresh(
        &self,
        pacman_conf: &Path,
        report_to: Option<(&WorkerClient, i32)>,
    ) -> Result<PathBuf> {
        let guard = self.open_pool().await?;
        let pool = guard.as_ref().expect("open_pool hands out an open pool");
        let root = pool.root();
        // One decision for "is it due" and "refresh it", or two builds
        // starting together both find it due.
        let Some(mut last) = self.refresh_gate(chroot::base_exists(&root)).await else {
            return Ok(root);
        };
        if chroot::base_exists(&root) && !refresh_due(last.map(|at| at.elapsed()), self.interval())
        {
            tracing::debug!("base chroot is current; not refreshing");
            return Ok(root);
        }
        // Across processes: `build-once` can share a pool with a running
        // worker. Busy means another process is refreshing now, and the
        // refresh is skipped the same way as above.
        let Some(_flock) = pool.try_refresh_lock() else {
            tracing::debug!("another process is refreshing the base chroot; using it as it is");
            return Ok(root);
        };
        // Whatever a previous refresh left: an unfinished candidate, or the
        // previous base past the exchange. `root` is whole whatever recovery
        // finds, so a recovery that fails is a failed refresh, not a failed
        // build: with a base, it is used as it is until the interval comes
        // round again; without one, it is made regardless, since what could
        // not be cleared is only ever `root.next`.
        if let Err(e) = pool.recover_refresh().await {
            let e = e.context("recovering an interrupted base refresh");
            if chroot::base_exists(&root) {
                warn_refresh(report_to, &e).await;
                *last = Some(Instant::now());
                return Ok(root);
            }
            tracing::warn!("{e:#}");
        }
        if !chroot::base_exists(&root) {
            // First start, or an operator removed the base: made in place, as
            // it always was, then counted against the total.
            if root.exists() {
                pool.discard_root().await?;
            }
            if let Err(e) = chroot::create_base(&root, pacman_conf).await {
                // Never left behind half made or uncounted: the next try
                // would take it as a base, refresh it forever and never
                // count it -- or, with no `usr` yet, `mkarchroot` would
                // refuse the directory and every build would fail.
                if let Err(cleanup) = pool.discard_root().await {
                    tracing::error!(
                        "could not remove the base chroot a failed creation left \
                         ({cleanup:#}); remove {} by hand",
                        root.display()
                    );
                }
                return Err(e);
            }
            // `mkarchroot` made the base a subvolume of its own; until it is
            // counted under the pool's total, the base's size is not.
            pool.charge_to_total(&root)
                .await
                .context("counting the new base chroot against the pool's total")?;
            *last = Some(Instant::now());
            return Ok(root);
        }
        // Snapshot, upgrade, check, swap. Any failure deletes the candidate
        // and leaves `root` exactly as it was; it is reported as a refresh
        // warning, and `last` is stamped anyway, so a refresh that fails waits
        // out the interval instead of running before every build. Builds are
        // not exposed to the stale base by this: each still runs its own
        // `makechrootpkg -u` in its snapshot, where the same failure shows up
        // in that build's log.
        let candidate = async {
            pool.snapshot_next().await?;
            chroot::upgrade_candidate(&pool.next_root()).await?;
            chroot::ensure_multilib(&pool.next_root()).await;
            chroot::check_candidate(&pool.next_root()).await?;
            // The swap is atomic; a refusal is a failed check like any other.
            pool.exchange_root()
                .context("swapping the refreshed base in")?;
            anyhow::Ok(())
        };
        if let Err(e) = candidate.await {
            pool.discard_next().await;
            warn_refresh(report_to, &e).await;
            *last = Some(Instant::now());
            return Ok(root);
        }
        // Past the exchange `root` is the new base. Retiring the old one can
        // only fail to a state the next recovery finishes, so a failure here
        // is logged, not returned.
        if let Err(e) = pool.retire_prev().await {
            tracing::warn!("could not retire the previous base chroot ({e:#})");
        }
        *last = Some(Instant::now());
        Ok(root)
    }

    /// Take a chroot for one build, limited to `limit` bytes of disk. Give it
    /// back with [`Self::release`], keep it with [`Self::keep`], or use
    /// [`Self::with_lease`].
    pub async fn acquire(&self, build_id: i32, limit: Option<u64>) -> Result<Lease> {
        let guard = self.open_pool().await?;
        let pool = guard.as_ref().expect("open_pool hands out an open pool");
        // Counted before the lease is taken, under the read lock: a pool is
        // only made again with the write lock and no build counted, so it can
        // never go while a lease is being taken from it.
        let active = ActiveLease::new(Arc::clone(&self.active));
        // Expired failures go before a new build's volumes come in, and kept
        // failures and the retired base go before a build that would not fit.
        pool.expire_kept(self.keep_failed()).await;
        if let Some(limit) = limit {
            pool.reclaim_room(limit).await;
        }
        let volumes = pool.lease(build_id, limit).await?;
        let lease = Lease {
            dir: pool.path().to_path_buf(),
            volumes,
            limit,
            _active: active,
        };
        // The guard goes before `release`, which takes the lock again: with a
        // writer queued in between, a second read would wait on it for good.
        drop(guard);
        if let Err(e) = std::fs::create_dir(lease.tmpdir()) {
            self.release(lease).await;
            return Err(e).context("making the build's temporary directory");
        }
        Ok(lease)
    }

    /// The in-process refresh exclusion. Without waiting when there is a base
    /// to lease meanwhile: a refresh in flight never holds up a build start,
    /// which snapshots the current base at once instead (`None`). The one
    /// exception is a pool with no base yet, which waits -- there is nothing
    /// to snapshot until `mkarchroot` is done.
    async fn refresh_gate(
        &self,
        base_exists: bool,
    ) -> Option<tokio::sync::MutexGuard<'_, Option<Instant>>> {
        match self.last_refresh.try_lock() {
            Ok(last) => Some(last),
            Err(_) if base_exists => None,
            Err(_) => Some(self.last_refresh.lock().await),
        }
    }

    /// A read guard on the pool, opened. Opened again if it was made again
    /// between the open and the lock.
    async fn open_pool(&self) -> Result<tokio::sync::RwLockReadGuard<'_, Option<Pool>>> {
        loop {
            self.open().await?;
            let guard = self.pool.read().await;
            if guard.is_some() {
                return Ok(guard);
            }
        }
    }

    /// Give a build's chroot back: both its subvolumes go, and the space they
    /// held is freed in the background by btrfs.
    ///
    /// Awaited rather than left to `Drop`: it has to happen *after* the
    /// build's child has been reaped, and drop order is a poor place to state
    /// that dependency.
    pub async fn release(&self, lease: Lease) {
        match self.pool.read().await.as_ref() {
            Some(pool) => pool.release(lease.volumes).await,
            // Not reachable: a lease comes from an open pool, and a pool is
            // only made again once no lease is held. The sweep would find it.
            None => drop(lease.volumes),
        }
    }

    /// Keep a failed build's chroot for `keep_for` instead of deleting it, for
    /// an operator to inspect, with its persistent `tree` moved beside it when
    /// it has one. Returns where, and until when. On an error the volumes are
    /// deleted instead of leaked, and the tree is either gone with them or
    /// still in place for the caller to discard.
    ///
    /// Like [`Self::release`], after the build's child has been reaped: what
    /// is kept is the state the build failed in.
    pub async fn keep(
        &self,
        lease: Lease,
        keep_for: Duration,
        tree: Option<&Path>,
    ) -> Result<aurcache_chroot::KeptBuild> {
        let kept = match self.pool.read().await.as_ref() {
            Some(pool) => pool.keep_build(lease.volumes, keep_for, tree).await,
            // Not reachable, as in `release`.
            None => {
                drop(lease.volumes);
                Err(anyhow::anyhow!("the storage pool is not open"))
            }
        };
        if let Err(e) = &kept {
            tracing::warn!("could not keep a failed build's chroot; deleted it instead ({e:#})");
        }
        kept
    }

    /// Run `body` with a chroot and give it back afterwards -- on the error
    /// path too, which is the half a caller forgets.
    pub async fn with_lease<T, F>(&self, build_id: i32, limit: Option<u64>, body: F) -> Result<T>
    where
        F: AsyncFnOnce(&Lease) -> Result<T>,
    {
        let lease = self.acquire(build_id, limit).await?;
        let out = body(&lease).await;
        self.release(lease).await;
        out
    }

    /// What a build's chroot and working space hold now. Empty when the pool
    /// is not open.
    pub async fn build_usage(&self, lease: &Lease) -> aurcache_chroot::BuildUsage {
        match self.pool.read().await.as_ref() {
            Some(pool) => pool.build_usage(&lease.volumes).await,
            None => aurcache_chroot::BuildUsage::default(),
        }
    }

    /// Why a build that failed may have failed for want of disk, in the terms
    /// of the setting to change. `None` when neither its quota nor the pool's
    /// total was reached.
    pub async fn disk_reason(&self, lease: &Lease) -> Option<String> {
        let guard = self.pool.read().await;
        let pool = guard.as_ref()?;
        // A write that did not fit leaves the group just short of its limit,
        // by up to the size of that write; a megabyte covers what makepkg and
        // compilers write at once.
        const SLACK: u64 = 1 << 20;
        disk_reason(pool.usage(lease.volumes.group()), pool.total_usage(), SLACK)
    }
}

/// A refresh that failed and left the base as it was: to the log, and to the
/// build that triggered it -- `None` for `build-once`, which has no server to
/// tell and no build id to file it under.
async fn warn_refresh(report_to: Option<(&WorkerClient, i32)>, e: &anyhow::Error) {
    tracing::warn!("base chroot refresh failed, keeping the current base:\n{e:#}");
    if let Some((client, build_id)) = report_to {
        report_warning(
            client,
            Some(build_id),
            &format!("base chroot refresh failed, keeping the current base:\n{e:#}"),
        )
        .await;
    }
}

/// Why opening the pool last failed, and when.
struct OpenFailure {
    at: Instant,
    error: String,
}

/// Whether the pool is the size its total asks for; see
/// [`Chroots::shrink_when_idle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PoolSize {
    /// It fits: builds may start.
    Fits,
    /// It holds more than its total allows and is waiting for the builds
    /// running now to finish before it is made again; nothing new starts.
    Draining,
}

/// See [`Chroots::disk_reason`].
fn disk_reason(build: Option<Usage>, total: Option<Usage>, slack: u64) -> Option<String> {
    if let Some(usage) = build.filter(|u| u.at_limit(slack)) {
        return Some(format!(
            "out of disk: the build reached its disk quota of {} (WORKER_BUILD_DISK_MAX)",
            gib(usage.limit.expect("at_limit implies a limit"))
        ));
    }
    if let Some(usage) = total.filter(|u| u.at_limit(slack)) {
        return Some(format!(
            "out of disk: this worker's storage is full ({} of WORKER_DISK_MAX); this build \
             may have been under its own quota",
            gib(usage.limit.expect("at_limit implies a limit"))
        ));
    }
    None
}

/// Whether the base chroot is due a refresh.
fn refresh_due(since_last: Option<Duration>, interval: Duration) -> bool {
    since_last.is_none_or(|elapsed| elapsed >= interval)
}

/// A byte count in GiB, for the log.
pub(crate) fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30))
}

/// One build's claim on a chroot, and on the workdir beside it.
pub struct Lease {
    dir: PathBuf,
    volumes: BuildVolumes,
    limit: Option<u64>,
    _active: ActiveLease,
}

/// Keep a lease counted through acquisition, use and asynchronous cleanup.
struct ActiveLease(Arc<AtomicUsize>);

impl ActiveLease {
    fn new(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::SeqCst);
        Self(active)
    }
}

impl Drop for ActiveLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Lease {
    /// The `-r` argument: the directory the chroots live in.
    #[must_use]
    pub fn chroot_dir(&self) -> &Path {
        &self.dir
    }

    /// The `-l` argument: what this build's chroot is called.
    #[must_use]
    pub fn label(&self) -> &str {
        self.volumes.label()
    }

    /// Where the build's source is extracted, and its packages land: in its
    /// own subvolume, counted against its quota.
    ///
    /// A directory of its own rather than the subvolume's top, which also
    /// holds [`Self::tmpdir`]: the extraction takes the one directory it finds
    /// as the package's, and must find nothing else there.
    #[must_use]
    pub fn srcdir(&self) -> PathBuf {
        self.volumes.data().join("src")
    }

    /// Where devtools makes its working directory for this build, and where
    /// the host-side source download writes: inside the build's own
    /// subvolume, so under its quota. See [`crate::chroot::devtools_in`].
    #[must_use]
    pub fn tmpdir(&self) -> PathBuf {
        self.volumes.data().join("tmp")
    }

    /// The disk this build may use, `None` when unlimited.
    #[must_use]
    pub const fn limit(&self) -> Option<u64> {
        self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_a_lease_holder_releases_its_count() {
        let active = Arc::new(AtomicUsize::new(0));
        let (started, holding) = tokio::sync::oneshot::channel();
        let task = {
            let active = Arc::clone(&active);
            tokio::spawn(async move {
                let _lease = ActiveLease::new(active);
                started.send(()).unwrap();
                std::future::pending::<()>().await;
            })
        };
        holding.await.unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    /// A worker that has just started refreshes whatever the interval says --
    /// it may have been down for a week -- and an interval of zero keeps the
    /// old behaviour of refreshing before every build.
    #[test]
    fn a_refresh_is_due_when_it_has_never_happened_or_has_aged_out() {
        let interval = Duration::from_secs(900);
        assert!(refresh_due(None, interval));
        assert!(!refresh_due(Some(Duration::from_secs(60)), interval));
        assert!(refresh_due(Some(Duration::from_secs(900)), interval));
        assert!(refresh_due(Some(Duration::from_secs(60)), Duration::ZERO));
    }

    /// A refresh in flight makes a job start lease at once -- unless there is
    /// no base yet, in which case there is nothing to snapshot and it waits.
    #[tokio::test]
    async fn a_refresh_in_flight_is_skipped_unless_the_base_is_missing() {
        use aurcache_chroot::{Backing, PoolConfig};
        use std::sync::Arc;
        use std::time::Duration as StdDuration;

        let chroots = Arc::new(Chroots::new(
            PoolConfig {
                backing: Backing::Image {
                    path: "/nonexistent/pool.img".into(),
                    reserve: false,
                },
                mountpoint: "/nonexistent".into(),
                total: 0,
                owner: aurcache_chroot::Owner::current(),
            },
            StdDuration::from_secs(900),
            None,
            aurcache_chroot::Owner::current(),
        ));
        // Free when nothing is refreshing.
        assert!(chroots.refresh_gate(true).await.is_some());

        // A refresh in flight, holding the exclusion.
        let held = chroots.last_refresh.lock().await;
        assert!(
            chroots.refresh_gate(true).await.is_none(),
            "with a base to lease, a start does not queue behind the refresh"
        );
        // With no base, a start queues behind it instead of snapshotting
        // nothing.
        let mut waiting = tokio::spawn({
            let chroots = Arc::clone(&chroots);
            async move { chroots.refresh_gate(false).await.is_some() }
        });
        assert!(
            tokio::time::timeout(StdDuration::from_millis(100), &mut waiting)
                .await
                .is_err(),
            "with no base, a start waits for the refresh"
        );
        drop(held);
        assert!(
            waiting.await.expect("the waiter finishes"),
            "the waiter proceeds once the refresh is done"
        );
    }

    /// The reason names the setting that was reached, and prefers the build's
    /// own quota: when both are full, raising the total alone would not help.
    #[test]
    fn a_disk_failure_names_the_quota_that_was_reached() {
        let gib = 1u64 << 30;
        let at = |used, limit| {
            Some(Usage {
                used,
                limit: Some(limit),
            })
        };
        let build = disk_reason(at(50 * gib, 50 * gib), at(100 * gib, 200 * gib), 0).unwrap();
        assert!(build.contains("WORKER_BUILD_DISK_MAX"), "{build}");
        assert!(build.contains("50.0 GiB"), "{build}");

        let total = disk_reason(at(10 * gib, 50 * gib), at(200 * gib, 200 * gib), 0).unwrap();
        assert!(total.contains("WORKER_DISK_MAX"), "{total}");

        assert_eq!(
            disk_reason(at(10 * gib, 50 * gib), at(20 * gib, 200 * gib), 0),
            None
        );
        assert_eq!(disk_reason(None, None, 0), None);
    }
}
