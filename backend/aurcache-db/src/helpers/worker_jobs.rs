//! Database helpers for the remote-worker job lifecycle: atomic claim, lease
//! renewal via heartbeat, and abandoning the builds a worker lost.

use crate::helpers::build_enqueue::{Pending, Queued};
use crate::helpers::builds::refresh_package_status;
use crate::helpers::time::now_secs;
use crate::lists::WorkerList;
use crate::prelude::{Builds, Packages, Workers};
use crate::{builds, packages, workers};
use aurcache_common::api::worker::ApprovalStatus;
use aurcache_common::build_state::{BuildState, BuildTrigger, EndReason};
use pacman_mirrors::platforms::Platform;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, FromQueryResult, QueryFilter, QueryOrder,
    QuerySelect, TransactionSession, TransactionTrait,
};

use std::collections::{HashMap, HashSet};

/// The columns the queue scans (`claim_job`, `waiting_reasons`) actually read.
///
/// The claim re-reads the winner by id after its compare-and-swap, and both
/// scans only rank — so selecting whole rows (including the large
/// `vcs_sources` JSON) on every poll is pure overhead.
#[derive(Debug, Clone, FromQueryResult)]
struct QueuedBuild {
    id: i32,
    pkg_id: i32,
    platform: Platform,
    start_time: Option<i64>,
}

impl QueuedBuild {
    async fn queued<C: ConnectionTrait>(db: &C) -> Result<Vec<Self>, DbErr> {
        Builds::find()
            .select_only()
            .column(builds::Column::Id)
            .column(builds::Column::PkgId)
            .column(builds::Column::Platform)
            .column(builds::Column::StartTime)
            .filter(builds::Column::Status.eq(BuildState::Enqueued))
            .into_model::<Self>()
            .all(db)
            .await
    }
}

/// The pkgbase of each queued build's package, by package id.
async fn package_names<C: ConnectionTrait>(
    db: &C,
    builds: &[QueuedBuild],
) -> Result<HashMap<i32, String>, DbErr> {
    Ok(Packages::find()
        .select_only()
        .column(packages::Column::Id)
        .column(packages::Column::Name)
        .filter(packages::Column::Id.is_in(builds.iter().map(|b| b.pkg_id)))
        .into_tuple::<(i32, String)>()
        .all(db)
        .await?
        .into_iter()
        .collect())
}

/// One approved worker's routing-relevant configuration, plus its live state.
#[derive(Debug)]
struct WorkerCap {
    id: i32,
    name: String,
    priority: i32,
    concurrency: i32,
    arches: Vec<String>,
    last_seen: Option<i64>,
    /// Builds this worker currently holds in the ACTIVE state.
    active: i32,
    /// Asked by an operator to take nothing new. Still approved, so it keeps
    /// its builds and its place in the routing rules; it just claims nothing.
    paused: bool,
}

/// A snapshot of the approved fleet, taken once per claim.
///
/// Deliberately *not* a long-lived cache. The fleet splits into a stable half
/// (affinity, priority, concurrency, arches — changes only on
/// re-register/approve/revoke) and a volatile half (`last_seen`, active build
/// counts — changes on every heartbeat and every claim). [`Fleet::available`]
/// needs the volatile half, so a round trip happens on every claim regardless;
/// caching the stable half separately would save nothing while adding
/// invalidation hooks, each a chance to route a job to a worker that cannot
/// build it.
#[derive(Debug, Default)]
struct Fleet {
    workers: Vec<WorkerCap>,
    /// pkgbase -> the approved workers that named it. Because affinity entries
    /// are exact names, this is a direct inversion of the worker rows: one pass,
    /// no matching, and no dependency on which builds are queued.
    affinity: HashMap<String, HashSet<i32>>,
}

/// The `workers` columns the router actually needs, as a named struct rather
/// than a wide tuple.
#[derive(Debug, FromQueryResult)]
struct WorkerRow {
    id: i32,
    name: String,
    priority: i32,
    concurrency: i32,
    arches: WorkerList,
    package_affinity: WorkerList,
    last_seen: Option<i64>,
    paused: bool,
}

impl Fleet {
    /// Load every approved worker plus its current ACTIVE build count.
    async fn load<C: ConnectionTrait>(db: &C) -> Result<Self, DbErr> {
        let rows: Vec<WorkerRow> = Workers::find()
            .select_only()
            .column(workers::Column::Id)
            .column(workers::Column::Name)
            .column(workers::Column::Priority)
            .column(workers::Column::Concurrency)
            .column(workers::Column::Arches)
            .column(workers::Column::PackageAffinity)
            .column(workers::Column::LastSeen)
            .column(workers::Column::Paused)
            .filter(workers::Column::Status.eq(ApprovalStatus::Approved))
            .into_model()
            .all(db)
            .await?;

        let counts: Vec<(Option<i32>, i64)> = Builds::find()
            .select_only()
            .column(builds::Column::WorkerId)
            .column_as(builds::Column::Id.count(), "cnt")
            .filter(builds::Column::Status.eq(BuildState::Active))
            .group_by(builds::Column::WorkerId)
            .into_tuple()
            .all(db)
            .await?;
        let active: HashMap<i32, i32> = counts
            .into_iter()
            .filter_map(|(id, n)| id.map(|id| (id, i32::try_from(n).unwrap_or(i32::MAX))))
            .collect();

        let mut fleet = Self::default();
        for row in rows {
            for pkg in row.package_affinity.into_vec() {
                fleet.affinity.entry(pkg).or_default().insert(row.id);
            }
            fleet.workers.push(WorkerCap {
                id: row.id,
                name: row.name,
                priority: row.priority,
                concurrency: row.concurrency,
                arches: row.arches.into_vec(),
                last_seen: row.last_seen,
                active: active.get(&row.id).copied().unwrap_or(0),
                paused: row.paused,
            });
        }
        Ok(fleet)
    }

    /// Names of the approved workers that declared affinity for a package,
    /// sorted so the explanation is stable between requests.
    fn affinity_holders(&self, pkg: &str) -> Vec<String> {
        let Some(ids) = self.affinity.get(pkg) else {
            return Vec::new();
        };
        let mut names: Vec<String> = self
            .workers
            .iter()
            .filter(|w| ids.contains(&w.id))
            .map(|w| w.name.clone())
            .collect();
        names.sort();
        names
    }

    fn worker(&self, id: i32) -> Option<&WorkerCap> {
        self.workers.iter().find(|w| w.id == id)
    }

    /// True when some approved worker has claimed affinity for this package, in
    /// which case only those workers may build it.
    fn reserved(&self, pkg: &str) -> bool {
        self.affinity.get(pkg).is_some_and(|ws| !ws.is_empty())
    }

    fn affine(&self, worker_id: i32, pkg: &str) -> bool {
        self.affinity
            .get(pkg)
            .is_some_and(|ws| ws.contains(&worker_id))
    }

    /// Whether the worker builds this architecture. Every worker that lists
    /// it competes for its builds on equal terms; how each builds it is not
    /// the router's concern.
    fn arch_ok(w: &WorkerCap, platform: &str) -> bool {
        w.arches.iter().any(|a| a == platform)
    }

    /// Whether this worker may build this job at all — a hard filter combining
    /// architecture routing with package affinity.
    fn capable(&self, w: &WorkerCap, platform: &str, pkg: &str) -> bool {
        Self::arch_ok(w, platform) && (!self.reserved(pkg) || self.affine(w.id, pkg))
    }

    /// Whether this worker was seen recently enough to count as up.
    fn live(w: &WorkerCap, now: i64, liveness_timeout: i64) -> bool {
        is_live(w.last_seen, now, liveness_timeout)
    }

    /// Whether this worker could pick a job up *right now*: up, not paused,
    /// and not already at its concurrency limit.
    ///
    /// Being paused counts here, because this is what priority hold-back asks: a
    /// paused worker will never take the job, so a lower-priority one must
    /// not wait for it.
    fn available(w: &WorkerCap, now: i64, liveness_timeout: i64) -> bool {
        Self::live(w, now, liveness_timeout) && !w.paused && w.active < w.concurrency
    }

    /// Whether a strictly higher-priority worker could take this job right now.
    ///
    /// Only *strictly* higher blocks, so equal-priority workers never hold each
    /// other back and an all-default fleet never blocks at all.
    fn blocked(
        &self,
        me: &WorkerCap,
        platform: &str,
        pkg: &str,
        now: i64,
        liveness_timeout: i64,
    ) -> bool {
        self.workers.iter().any(|w| {
            w.id != me.id
                && w.priority > me.priority
                && self.capable(w, platform, pkg)
                && Self::available(w, now, liveness_timeout)
        })
    }
}

/// Where a queued build stands in one worker's claim order, compared field by
/// field: affine jobs first, then the oldest, then the id.
///
/// The first is negated because `false < true`. A worker that *is* affine
/// for a package should take that job ahead of one anybody could have taken,
/// since it may be the only worker that can.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct ClaimRank {
    not_affine: bool,
    queued_at: i64,
    id: i32,
}

/// Whether a worker last seen at `last_seen` counts as up at `now`.
///
/// One that has never checked in does not, which is a different statement from
/// one that checked in and stopped -- `last_seen` tells those apart and this
/// deliberately does not.
#[must_use]
pub fn is_live(last_seen: Option<i64>, now: i64, liveness_timeout: i64) -> bool {
    last_seen.is_some_and(|seen| now - seen <= liveness_timeout)
}

/// Atomically claim the next buildable job for `worker_id`.
///
/// Routing has three layers (see `design/implemented/worker-routing.md`):
///
/// 1. **Affinity** (hard) — a package named by any approved worker may only be
///    built by workers that name it. Ignores liveness: handing an affine job to
///    a worker without the credential fails, where waiting merely waits.
/// 2. **Architecture** (hard) — only a worker that lists the build's
///    architecture.
/// 3. **Priority** (soft) — a worker declines a job while a strictly
///    higher-priority worker is live and under its concurrency limit, until the
///    job has waited `spill_delay_secs`.
///
/// On top of those, a worker is never handed a second build of a package it is
/// already building. The two would share one `SRCDEST` — it is keyed by pkgbase
/// and not by platform — and fetch into it at once. The worker refuses to run
/// them concurrently anyway (`aurcache_worker::srcdest_lock`), so offering the
/// job would only park a claimed build against its own timeout while it waited
/// its turn. Per *worker*, deliberately: two workers building different
/// platforms of one package share nothing, and that is the parallelism
/// multi-arch exists for.
///
/// Arches, affinity and priority all come from the worker's stored row rather
/// than from the request, because each worker's decision depends on what *other*
/// workers declared: they must be read from one consistent source.
///
/// The final transition is a conditional `UPDATE ... WHERE status = ENQUEUED`,
/// so two workers racing for the same build see exactly one `rows_affected == 1`.
pub async fn claim_job<C: ConnectionTrait + sea_orm::TransactionTrait>(
    db: &C,
    worker_id: i32,
    lease_ttl_secs: i64,
    spill_delay_secs: i64,
    liveness_timeout_secs: i64,
) -> Result<Option<builds::Model>, DbErr> {
    // It reads the fleet and the queue before the claim writes.
    let txn = crate::helpers::txn::begin_write(db).await?;
    let result: Result<_, DbErr> = async {
        let db = &txn;
        let fleet = Fleet::load(db).await?;
        // Not approved (or vanished mid-request): nothing is claimable. Paused:
        // nothing new, by the operator's request -- which is the whole of pause.
        let Some(me) = fleet.worker(worker_id).filter(|me| !me.paused) else {
            return Ok(None);
        };

        let candidates: Vec<QueuedBuild> = QueuedBuild::queued(db).await?;
        if candidates.is_empty() {
            return Ok(None);
        }

        // Packages this worker already has in flight. `ACTIVE` alone: a build that
        // has moved on to `PUBLISHING` has ended its lease, and its worker has
        // finished with the sources — the same predicate the rest of lease policing
        // uses.
        let busy_pkgs: HashSet<i32> = Builds::find()
            .select_only()
            .column(builds::Column::PkgId)
            .filter(builds::Column::Status.eq(BuildState::Active))
            .filter(builds::Column::WorkerId.eq(worker_id))
            .into_tuple::<i32>()
            .all(db)
            .await?
            .into_iter()
            .collect();

        let names = package_names(db, &candidates).await?;

        let now = now_secs();
        let mut ranked: Vec<ClaimRank> = Vec::new();
        for build in &candidates {
            let Some(pkg) = names.get(&build.pkg_id) else {
                continue;
            };
            // Leave it queued rather than parking it here: another worker has its
            // own `SRCDEST` and can take it now.
            if busy_pkgs.contains(&build.pkg_id) {
                continue;
            }
            let platform = build.platform.as_str();
            if !fleet.capable(me, platform, pkg) {
                continue;
            }
            let age = now - build.start_time.unwrap_or(now);
            if age < spill_delay_secs
                && fleet.blocked(me, platform, pkg, now, liveness_timeout_secs)
            {
                continue;
            }
            ranked.push(ClaimRank {
                not_affine: !fleet.affine(me.id, pkg),
                queued_at: build.start_time.unwrap_or(0),
                id: build.id,
            });
        }
        ranked.sort_unstable();

        for ClaimRank { id, .. } in ranked {
            let res = Builds::update_many()
                .col_expr(builds::Column::Status, BuildState::Active.into())
                .col_expr(builds::Column::WorkerId, worker_id.into())
                .col_expr(
                    builds::Column::LeaseExpiresAt,
                    (now + lease_ttl_secs).into(),
                )
                .col_expr(builds::Column::StartTime, now.into())
                .filter(builds::Column::Id.eq(id))
                .filter(builds::Column::Status.eq(BuildState::Enqueued))
                .exec(db)
                .await?;
            if res.rows_affected == 1 {
                // Only after the claim above has actually won: that update is the
                // compare-and-swap deciding which worker gets the build, so doing
                // this first would announce a build that another worker took.
                if let Some(pkg_id) = candidates.iter().find(|b| b.id == id).map(|b| b.pkg_id) {
                    refresh_package_status(db, pkg_id).await?;
                }
                return Builds::find_by_id(id).one(db).await;
            }
        }
        Ok(None)
    }
    .await;
    let result = result?;
    txn.commit().await?;
    Ok(result)
}

/// Result of processing a heartbeat: builds that were reconciled away from the
/// worker because it stopped reporting them while still alive, and builds the
/// worker must stop because they are no longer ACTIVE under it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HeartbeatOutcome {
    /// Ids whose lease was renewed.
    pub renewed: Vec<i32>,
    /// Builds that were owned + active but absent from the report, now
    /// abandoned: the worker lost them without saying so.
    pub dropped: Vec<Abandoned>,
    /// Ids the worker reported but that are no longer ACTIVE-and-owned by it
    /// (missing, terminal, or handed to someone else). The worker must stop
    /// these.
    pub cancel_requested: Vec<i32>,
}

/// Renew leases for the builds a worker reports as active, and abandon any
/// build still owned + active by this worker that it *didn't* report: the
/// worker silently dropped it, so it gets a fresh attempt now rather than
/// after its lease runs out (see [`abandon_build`]).
pub async fn heartbeat<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    worker_id: i32,
    active_build_ids: &[i32],
    lease_ttl_secs: i64,
    max_attempts: i32,
) -> Result<HeartbeatOutcome, DbErr> {
    let now = now_secs();
    let mut outcome = HeartbeatOutcome::default();

    // Every build this worker currently owns in the ACTIVE state.
    let owned: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::WorkerId.eq(worker_id))
        .filter(builds::Column::Status.eq(BuildState::Active))
        .all(db)
        .await?;

    // Renewed in one statement, not one per build: the filters are the same
    // per row (owner, still active), so an `IN` set is exactly equivalent —
    // including the old code's not checking `rows_affected` here. A worker
    // owns a handful of builds, so this is tidiness rather than rescue.
    let mut renew = Vec::new();
    for build in owned {
        let id = build.id;
        if active_build_ids.contains(&id) {
            renew.push(id);
        } else {
            outcome
                .dropped
                .extend(abandon_build(db, &build, EndReason::Dropped, max_attempts).await?);
        }
    }
    if !renew.is_empty() {
        outcome.renewed.extend(renew.iter().copied());
        Builds::update_many()
            .col_expr(
                builds::Column::LeaseExpiresAt,
                (now + lease_ttl_secs).into(),
            )
            .filter(builds::Column::Id.is_in(renew))
            .filter(builds::Column::WorkerId.eq(worker_id))
            .filter(builds::Column::Status.eq(BuildState::Active))
            .exec(db)
            .await?;
    }

    // The abort list: every reported id that is missing, terminal, or owned by
    // someone else is a build this worker's next tick must stop. `status =
    // ACTIVE AND owner = me` is the only state with nothing to say. A benign
    // race: an id may be flagged the frame after the worker completed it, since
    // the id only leaves the worker's `active` set once the completion report
    // returns — the flag is only read inside the build's kill loop, which has
    // already exited by then.
    if !active_build_ids.is_empty() {
        let reported_rows: HashMap<i32, builds::Model> = Builds::find()
            .filter(builds::Column::Id.is_in(active_build_ids.iter().copied()))
            .all(db)
            .await?
            .into_iter()
            .map(|b| (b.id, b))
            .collect();
        for id in active_build_ids {
            let cancel = match reported_rows.get(id) {
                None => true,
                Some(b) => b.status != BuildState::Active || b.worker_id != Some(worker_id),
            };
            if cancel {
                outcome.cancel_requested.push(*id);
            }
        }
    }
    Ok(outcome)
}

/// The condition that `observed`, an `ACTIVE` build, is still held exactly as
/// it was read: same owner, same lease revision.
///
/// The pin every reclaim writes under, so a heartbeat renewal or a completion
/// landing between the read and the write turns the write into a no-op.
fn still_held(observed: &builds::Model) -> sea_orm::Condition {
    let owner = match observed.worker_id {
        Some(w) => builds::Column::WorkerId.eq(w),
        None => builds::Column::WorkerId.is_null(),
    };
    let lease = match observed.lease_expires_at {
        Some(l) => builds::Column::LeaseExpiresAt.eq(l),
        None => builds::Column::LeaseExpiresAt.is_null(),
    };
    sea_orm::Condition::all()
        .add(builds::Column::Id.eq(observed.id))
        .add(builds::Column::Status.eq(BuildState::Active))
        .add(owner)
        .add(lease)
}

/// Fail a build for good and queue a *fresh* build in its place, in one
/// transaction.
///
/// The one way a build is taken back from a worker: a build row is one
/// attempt, never more, so an abandoned row — silent worker, stale lease, hung
/// past the backstop, dropped by its worker, or held by a revoked one — is
/// written terminal `FAILED` and a new row is inserted for `(pkg_id,
/// platform)` while the derived retry budget allows. Nothing about the old attempt carries over:
/// the retry gets the current package version, its own log, and whatever worker
/// claims it.
///
/// ## The transaction
///
/// The terminal-status write is the decision point and everything downstream is
/// atomic with it:
///
/// 1. **CAS the row to `FAILED`**, pinned to the exact revision the caller
///    observed (`status = ACTIVE AND worker_id = ? AND lease_expires_at = ?`;
///    see [`still_held`]). The pin includes the observed
///    **lease revision**: a heartbeat renewal landing between the reaper's read
///    and its write must make this a no-op (0 rows), or a healthy worker's live
///    build would be terminally failed. `worker_id` is kept — the record of
///    who ran the attempt — and only the lease is cleared.
/// 2. **The package's status follows** ([`refresh_package_status`]): the
///    failure, or the retry queued in its place.
/// 3. **Decide the budget** by walking the package's most recent build rows
///    *on the same platform*: count the trailing consecutive `TimeoutRetry`
///    rows whose `end_reason` spends a retry ([`EndReason::spends_retry`]). Per platform because each
///    platform has its own chain of retries, and interleaving two would spend
///    one budget between them. The just-failed row is part of the walk. A `user` or
///    `auto_update` trigger, a success, or any other `end_reason` breaks the
///    chain and restores the budget.
/// 4. **Insert the retry** (`ENQUEUED`, trigger `TimeoutRetry`, version from
///    the *current* package metadata) while the count is below `max_attempts`;
///    if a pending build already exists — a concurrent auto-update queued one —
///    the insert is skipped and that build stands in for it.
///
/// If the CAS wins no row, everything rolls back and nothing downstream runs:
/// there is no "row failed but package still Building". That is the `None`: the row
/// moved on (renewed, completed, or reclaimed) between the caller's read and
/// this write.
pub async fn abandon_build<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    observed: &builds::Model,
    end_reason: EndReason,
    max_attempts: i32,
) -> Result<Option<Abandoned>, DbErr> {
    let now = now_secs();
    let txn = db.begin().await?;

    // 1. The CAS. 0 rows is "someone else resolved it in the gap" and the whole
    //    thing stops here.
    let cas = Builds::update_many()
        .col_expr(builds::Column::Status, BuildState::Failed.into())
        .col_expr(builds::Column::EndTime, Some(now).into())
        .col_expr(builds::Column::EndReason, Some(end_reason).into())
        .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into())
        .filter(still_held(observed))
        .exec(&txn)
        .await?;
    if cas.rows_affected == 0 {
        txn.rollback().await?;
        return Ok(None);
    }

    let pkg = Packages::find_by_id(observed.pkg_id).one(&txn).await?;

    // 3. The derived budget: trailing consecutive TimeoutRetry rows that ended
    //    in abandonment. Limited to `max_attempts` — once the run reaches the
    //    budget the answer is "spent", and the walk never has to look further.
    let mut futile_run = 0i32;
    let recent: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::PkgId.eq(observed.pkg_id))
        .filter(builds::Column::Platform.eq(observed.platform.as_str()))
        .order_by(builds::Column::Number, sea_orm::Order::Desc)
        .limit(max_attempts.max(1) as u64)
        .all(&txn)
        .await?;
    for row in &recent {
        if row.trigger == BuildTrigger::TimeoutRetry
            && row.end_reason.is_some_and(EndReason::spends_retry)
        {
            futile_run += 1;
        } else {
            break;
        }
    }

    // 4. Insert (or adopt) the fresh build, while the budget allows. An
    //    existing pending build -- a concurrent auto-update queued one once
    //    this row stopped conflicting -- stands in for it.
    let mut retry = Retry::Spent;
    if futile_run < max_attempts
        && let Some(pkg) = &pkg
    {
        let enqueue = crate::helpers::build_enqueue::enqueue_build_if_missing(
            &txn,
            observed.pkg_id,
            observed.platform,
            pkg.upstream_version.as_deref().unwrap_or_default(),
            now,
            Pending::Enqueued,
            BuildTrigger::TimeoutRetry,
        )
        .await?;
        retry = match enqueue.queued {
            Queued::Inserted | Queued::Promoted => Retry::Queued(enqueue.build.id),
            Queued::Existing => Retry::Adopted(enqueue.build.id),
        };
    }
    refresh_package_status(&txn, observed.pkg_id).await?;

    txn.commit().await?;
    Ok(Some(Abandoned {
        build_id: observed.id,
        number: observed.number,
        pkgbase: pkg.map(|p| p.name),
        end_reason,
        worker_id: observed.worker_id,
        retry,
    }))
}

/// Why an `ENQUEUED` build is not being picked up by anyone.
///
/// Only ever set for builds that *no approved worker can currently take*.
/// Ordinary queueing behind a busy worker is not a reason and yields `None`;
/// otherwise every queued build would carry a scary-looking explanation.
/// Re-exported from `aurcache-common`, where it lives so the API and a
/// frontend can share it.
pub use aurcache_common::api::waiting::WaitingReason;

/// Explain every `ENQUEUED` build that no approved worker can currently claim.
///
/// Without this an affinity-reserved build that is stalled because its worker is
/// offline looks exactly like a build waiting behind a busy queue — which is the
/// single most likely way package affinity wastes someone's afternoon.
pub async fn waiting_reasons<C: ConnectionTrait>(
    db: &C,
    liveness_timeout_secs: i64,
) -> Result<HashMap<i32, WaitingReason>, DbErr> {
    let queued: Vec<QueuedBuild> = QueuedBuild::queued(db).await?;
    if queued.is_empty() {
        return Ok(HashMap::new());
    }

    let fleet = Fleet::load(db).await?;
    let names = package_names(db, &queued).await?;

    let now = now_secs();
    let mut out = HashMap::new();
    for build in &queued {
        let Some(pkg) = names.get(&build.pkg_id) else {
            continue;
        };
        let platform = build.platform.as_str();
        let capable: Vec<&WorkerCap> = fleet
            .workers
            .iter()
            .filter(|w| fleet.capable(w, platform, pkg))
            .collect();

        let reason = if capable.is_empty() {
            if fleet.reserved(pkg) {
                // Reserved, and nothing that can take it: name the holders so an
                // operator knows which machine to bring back or revoke.
                WaitingReason::Affinity {
                    workers: fleet.affinity_holders(pkg),
                }
            } else {
                WaitingReason::Arch {
                    arch: platform.to_string(),
                }
            }
        } else if capable
            .iter()
            .any(|w| Fleet::live(w, now, liveness_timeout_secs) && !w.paused)
        {
            // Someone capable is alive and taking work; this is just a queue.
            continue;
        } else if capable
            .iter()
            .any(|w| Fleet::live(w, now, liveness_timeout_secs))
        {
            // Up, but every one of them was asked to take nothing new. Named,
            // because resuming one of them is the remedy.
            let mut workers: Vec<String> = capable
                .iter()
                .filter(|w| Fleet::live(w, now, liveness_timeout_secs))
                .map(|w| w.name.clone())
                .collect();
            workers.sort();
            WaitingReason::Paused { workers }
        } else if fleet.reserved(pkg) {
            WaitingReason::Affinity {
                workers: fleet.affinity_holders(pkg),
            }
        } else {
            WaitingReason::Offline
        };
        out.insert(build.id, reason);
    }
    Ok(out)
}

/// Abandon every build a worker still holds in the `ACTIVE` state, each with
/// a fresh attempt in its place (see [`abandon_build`]).
///
/// Called when a worker is revoked: from that moment the auth guard refuses it,
/// so it can never report those builds complete. Without this they would sit
/// `ACTIVE` until the lease reaper noticed, up to `LEASE_TTL`. A revocation
/// does not spend the builds' retry budget: it was a decision about the
/// machine, not the build.
pub async fn abandon_worker_builds<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    worker_id: i32,
    max_attempts: i32,
) -> Result<Vec<Abandoned>, DbErr> {
    let owned: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::WorkerId.eq(worker_id))
        .filter(builds::Column::Status.eq(BuildState::Active))
        .all(db)
        .await?;

    let mut abandoned = Vec::new();
    for build in owned {
        // The same pinned write as the reaper's, so a build that completed
        // between the select above and this write is left alone.
        abandoned.extend(abandon_build(db, &build, EndReason::WorkerRevoked, max_attempts).await?);
    }
    Ok(abandoned)
}

impl Abandoned {
    /// The build as everything outside the database names it; `None` once its
    /// package is gone.
    #[must_use]
    pub fn build_ref(&self) -> Option<aurcache_common::api::log::BuildRef> {
        Some(aurcache_common::api::log::BuildRef {
            pkgbase: self.pkgbase.clone()?,
            number: self.number,
        })
    }
}

/// One build that was abandoned: failed for good, with a fresh attempt in its
/// place when the budget allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Abandoned {
    pub build_id: i32,
    /// The build's number within its package, which names its log.
    pub number: i32,
    /// `None` when the package row is already gone, and its logs with it.
    pub pkgbase: Option<String>,
    /// Why the server ended it, as recorded on the row.
    pub end_reason: EndReason,
    /// The worker that held it, if any did.
    pub worker_id: Option<i32>,
    /// What takes its place.
    pub retry: Retry,
}

/// What takes an abandoned build's place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    /// A fresh build, queued for this.
    Queued(i32),
    /// A build already pending for the same package and platform -- queued
    /// meanwhile by something else -- which stands in for the retry.
    Adopted(i32),
    /// No further attempt: the retry budget is spent, or the package is gone.
    Spent,
}

/// Reaper pass: reclaim `ACTIVE` builds whose owning worker went silent, and
/// backstop builds that have run implausibly long. Returns every build it
/// abandoned.
///
/// A build is reaped when either:
/// * its lease has expired (`lease_expires_at < now`, or `NULL` — no live
///   lease) — the worker missed enough heartbeats that we treat it as lost; or
/// * it has been `ACTIVE` past the backstop deadline
///   (`start_time + max_build_age < now`) even if a lease still appears fresh —
///   covers a hung build whose worker keeps heartbeating.
///
/// Each reaped build goes through [`abandon_build`]: one transaction fails the
/// row for good (keeping the `worker_id` that ran it) and queues a *fresh*
/// build while the derived [`BuildTrigger::TimeoutRetry`] budget allows —
/// never a requeue of the same row. The fresh row's version comes from the
/// current package metadata, matching every other enqueue path.
///
/// `now` and `max_build_age` (`MAX_BUILD_DURATION + grace`, in seconds) are
/// passed in so callers stay testable and can source them from settings.
pub async fn reap_expired_builds<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    now: i64,
    max_attempts: i32,
    max_build_age: i64,
) -> Result<Vec<Abandoned>, DbErr> {
    let backstop_before = now - max_build_age;
    let candidates: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::Status.eq(BuildState::Active))
        .filter(
            sea_orm::Condition::any()
                .add(builds::Column::LeaseExpiresAt.lt(now))
                .add(builds::Column::LeaseExpiresAt.is_null())
                .add(builds::Column::StartTime.lt(backstop_before)),
        )
        .all(db)
        .await?;

    let mut abandoned = Vec::new();
    for build in candidates {
        // `abandon_build` pins the CAS to the row revision observed here
        // (`status`, `worker_id`, `lease_expires_at`), so a build a concurrent
        // heartbeat renewed or a completion resolved in the gap since the
        // select above is skipped rather than clobbered. The backstop deadline
        // outranks the lease for choosing the end reason: a build past it is
        // too long whether or not it was still heartbeating.
        let over_backstop = build.start_time.is_some_and(|t| t < backstop_before);
        let end_reason = if over_backstop {
            EndReason::MaxDuration
        } else {
            EndReason::LeaseExpired
        };
        abandoned.extend(abandon_build(db, &build, end_reason, max_attempts).await?);
    }
    Ok(abandoned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection, PaginatorTrait};
    use sea_orm_migration::MigratorTrait;

    // Spelled out for the SQL fixtures, which interpolate them by name.
    const ACTIVE_BUILD: i32 = BuildState::Active.as_i32();
    const ENQUEUED_BUILD: i32 = BuildState::Enqueued.as_i32();
    const FAILED_BUILD: i32 = BuildState::Failed.as_i32();
    const SUCCESSFUL_BUILD: i32 = BuildState::Successful.as_i32();

    const LEASE: i64 = 60;
    const SPILL: i64 = 60;
    const LIVENESS: i64 = 60;

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    /// Ids of the abandoned builds failed for good, with no replacement.
    fn failed(abandoned: &[Abandoned]) -> Vec<i32> {
        abandoned
            .iter()
            .filter(|a| a.retry == Retry::Spent)
            .map(|a| a.build_id)
            .collect()
    }

    /// Ids of the abandoned builds that got a fresh attempt in their place.
    fn retried(abandoned: &[Abandoned]) -> Vec<i32> {
        abandoned
            .iter()
            .filter(|a| a.retry != Retry::Spent)
            .map(|a| a.build_id)
            .collect()
    }

    /// An approved worker row. Defaults are a live, idle, native-x86_64 worker
    /// with no affinity and no priority — i.e. one that neither reserves
    /// anything nor blocks anyone.
    struct W {
        id: i32,
        arches: &'static str,
        affinity: &'static str,
        priority: i32,
        concurrency: i32,
        status: ApprovalStatus,
        /// `None` means "never seen", which reads as not live.
        last_seen: Option<i64>,
    }

    impl Default for W {
        fn default() -> Self {
            Self {
                id: 1,
                arches: "x86_64",
                affinity: "",
                priority: 0,
                concurrency: 1,
                status: ApprovalStatus::Approved,
                last_seen: Some(now_secs()),
            }
        }
    }

    async fn worker(db: &DatabaseConnection, w: W) {
        let last_seen = w
            .last_seen
            .map_or_else(|| "NULL".to_string(), |t| t.to_string());
        db.execute_unprepared(&format!(
            "INSERT INTO workers (id, name, status, cert_fingerprint, arches, \
             package_affinity, priority, concurrency, last_seen, settings_declaration) \
             VALUES ({}, 'w{}', '{}', 'fp{}', '{}', '{}', {}, {}, {last_seen}, '[]')",
            w.id, w.id, w.status, w.id, w.arches, w.affinity, w.priority, w.concurrency
        ))
        .await
        .unwrap();
    }

    /// Enqueue a build of package `name` at `start` (epoch seconds).
    async fn enqueue_pkg(db: &DatabaseConnection, id: i32, name: &str, platform: &str, start: i64) {
        // A partial unique index forbids two pending builds for the same
        // (pkg_id, platform); give every build its own package.
        db.execute_unprepared(&format!(
            "INSERT INTO packages (id, name, status, out_of_date, build_flags, platforms, \
                source_data, directly_requested) \
             VALUES ({id}, '{name}', {ENQUEUED_BUILD}, 0, '', '{platform}', \
                '{{\"type\":\"aur\",\"name\":\"{name}\"}}', 1)"
        ))
        .await
        .unwrap();
        db.execute_unprepared(&format!(
            "INSERT INTO builds (id, pkg_id, status, start_time, platform, version) \
             VALUES ({id}, {id}, {ENQUEUED_BUILD}, {start}, '{platform}', '1.0')"
        ))
        .await
        .unwrap();
    }

    async fn enqueue(db: &DatabaseConnection, id: i32, platform: &str, start: i64) {
        enqueue_pkg(db, id, &format!("p{id}"), platform, start).await;
    }

    /// A package's status, without loading the rest of the row: the fixtures
    /// here insert only `(id, name)`, so the other columns are NULL and
    /// decoding a full model fails on them.
    async fn pkg_status(db: &DatabaseConnection, id: i32) -> BuildState {
        Packages::find_by_id(id)
            .select_only()
            .column(packages::Column::Status)
            .into_tuple::<BuildState>()
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn claim(db: &DatabaseConnection, worker_id: i32) -> Option<builds::Model> {
        claim_job(db, worker_id, LEASE, SPILL, LIVENESS)
            .await
            .unwrap()
    }

    /// One package with a build queued per platform, and a worker that could
    /// take either. `SRCDEST` is keyed by pkgbase and not by platform, so the
    /// two would share one source directory.
    async fn enqueue_second_platform(db: &DatabaseConnection, build_id: i32, pkg_id: i32) {
        db.execute_unprepared(&format!(
            "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, number) \
             VALUES ({build_id}, {pkg_id}, {ENQUEUED_BUILD}, 100, 'aarch64', '1.0', 2)"
        ))
        .await
        .unwrap();
    }

    /// A worker is never handed a second build of a package it is already
    /// building: the two would fetch into one `SRCDEST`. The worker refuses to
    /// run them at once regardless, so offering it would park a claimed build
    /// against its own timeout while it waited its turn.
    #[tokio::test]
    async fn a_worker_is_not_offered_a_second_build_of_what_it_is_building() {
        let db = setup().await;
        worker(
            &db,
            W {
                arches: "x86_64,aarch64",
                concurrency: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 10, "x86_64", 100).await;
        enqueue_second_platform(&db, 11, 10).await;

        let first = claim(&db, 1)
            .await
            .expect("the first platform is claimable");
        assert_eq!(first.id, 10);

        assert!(
            claim(&db, 1).await.is_none(),
            "the other platform of a package this worker is building must stay queued"
        );
    }

    /// Per worker, not fleet-wide: two workers have separate source caches, and
    /// building a package's platforms at once is what multi-arch is for.
    #[tokio::test]
    async fn another_worker_may_take_the_other_platform() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                arches: "x86_64,aarch64",
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                arches: "aarch64",
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 10, "x86_64", 100).await;
        enqueue_second_platform(&db, 11, 10).await;

        assert_eq!(claim(&db, 1).await.expect("first platform").id, 10);
        assert_eq!(
            claim(&db, 2).await.expect("second platform elsewhere").id,
            11,
            "another worker has its own SRCDEST and is free to take it"
        );
    }

    /// The exclusion follows `ACTIVE`, like the rest of lease policing: once a
    /// build is publishing, its lease is over and its worker has finished with
    /// the sources.
    #[tokio::test]
    async fn publishing_releases_the_package_for_the_next_platform() {
        let db = setup().await;
        worker(
            &db,
            W {
                arches: "x86_64,aarch64",
                concurrency: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 10, "x86_64", 100).await;
        enqueue_second_platform(&db, 11, 10).await;

        claim(&db, 1).await.expect("the first platform");
        db.execute_unprepared(&format!(
            "UPDATE builds SET status = {} WHERE id = 10",
            BuildState::Publishing.as_i32()
        ))
        .await
        .unwrap();

        assert_eq!(
            claim(&db, 1).await.expect("the second platform").id,
            11,
            "a publishing build no longer holds the package's sources"
        );
    }

    /// The package's own status has to follow the build's, because that is the
    /// one the packages list renders. It used to be written on enqueue and
    /// again on completion, with nothing in between -- so for the whole
    /// duration of a build the package claimed to be waiting for it to start.
    #[tokio::test]
    async fn a_failed_package_refresh_rolls_back_the_claim() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 1, "x86_64", 100).await;
        db.execute_unprepared("CREATE TRIGGER reject_status BEFORE UPDATE OF status ON packages BEGIN SELECT RAISE(ABORT, 'status failed'); END").await.unwrap();
        assert!(claim_job(&db, 1, LEASE, SPILL, LIVENESS).await.is_err());
        let build = Builds::find_by_id(1).one(&db).await.unwrap().unwrap();
        assert_eq!(build.status, BuildState::Enqueued);
        assert_eq!(build.worker_id, None);
        assert_eq!(build.lease_expires_at, None);
    }

    #[tokio::test]
    async fn claiming_a_build_marks_its_package_as_building() {
        let db = setup().await;
        worker(&db, W::default()).await;
        enqueue(&db, 10, "x86_64", 100).await;
        db.execute_unprepared(&format!(
            "UPDATE packages SET status = {ENQUEUED_BUILD} WHERE id = 10"
        ))
        .await
        .unwrap();

        let claimed = claim(&db, 1).await.unwrap();
        assert_eq!(claimed.status, BuildState::Active);

        assert_eq!(
            pkg_status(&db, 10).await,
            BuildState::Active,
            "the package still reports the status it had before the build started"
        );
    }

    /// Only the package whose build was actually claimed. A claim moves one
    /// build; announcing every queued package as building would be a different
    /// wrong answer.
    #[tokio::test]
    async fn claiming_leaves_other_packages_alone() {
        let db = setup().await;
        worker(&db, W::default()).await;
        enqueue(&db, 10, "x86_64", 100).await;
        enqueue(&db, 11, "x86_64", 200).await;
        db.execute_unprepared(&format!(
            "UPDATE packages SET status = {ENQUEUED_BUILD} WHERE id IN (10, 11)"
        ))
        .await
        .unwrap();

        // The older job is the one that gets taken.
        let claimed = claim(&db, 1).await.unwrap();
        assert_eq!(claimed.pkg_id, 10);

        assert_eq!(pkg_status(&db, 11).await, BuildState::Enqueued);
    }

    // ---------------------------------------------------------------- arches

    #[tokio::test]
    async fn claims_oldest_matching_arch() {
        let db = setup().await;
        worker(&db, W::default()).await;
        enqueue(&db, 10, "x86_64", 200).await;
        enqueue(&db, 11, "x86_64", 100).await;
        enqueue(&db, 12, "aarch64", 50).await;

        let claimed = claim(&db, 1).await.unwrap();
        // Oldest x86_64 job wins; the older aarch64 job is not buildable.
        assert_eq!(claimed.id, 11);
        assert_eq!(claimed.status, BuildState::Active);
        assert_eq!(claimed.worker_id, Some(1));
        assert!(claimed.lease_expires_at.is_some());
    }

    #[tokio::test]
    async fn claim_is_atomic_no_double_handout() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 20, "x86_64", 100).await;

        assert!(claim(&db, 1).await.is_some());
        assert!(claim(&db, 2).await.is_none());
    }

    #[tokio::test]
    async fn unknown_or_unapproved_worker_claims_nothing() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                status: ApprovalStatus::Revoked,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 21, "x86_64", 100).await;

        assert!(claim(&db, 1).await.is_none(), "revoked worker claimed");
        assert!(claim(&db, 99).await.is_none(), "unknown worker claimed");
    }

    /// Only workers that list an architecture take its builds, and all of
    /// them compete: the one that asks first gets it.
    #[tokio::test]
    async fn workers_listing_an_arch_compete_for_it() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                arches: "x86_64,aarch64",
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 3,
                arches: "aarch64",
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 40, "aarch64", 50).await;
        enqueue(&db, 41, "aarch64", 60).await;

        assert!(
            claim(&db, 1).await.is_none(),
            "took an arch it does not list"
        );
        assert_eq!(claim(&db, 2).await.unwrap().id, 40);
        assert_eq!(claim(&db, 3).await.unwrap().id, 41);
    }

    // -------------------------------------------------------------- affinity

    #[tokio::test]
    async fn affinity_reserves_package_to_declaring_worker() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 60, "unreal-engine", "x86_64", 100).await;

        assert!(claim(&db, 1).await.is_none(), "non-affine worker claimed");
        assert_eq!(claim(&db, 2).await.unwrap().id, 60);
    }

    #[tokio::test]
    async fn package_nobody_claims_is_unaffected_by_affinity_lists() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 61, "hello", "x86_64", 100).await;

        assert_eq!(claim(&db, 1).await.unwrap().id, 61);
    }

    /// Exact matching only: no wildcards, and no accidental prefix reservation.
    #[tokio::test]
    async fn affinity_matches_package_names_exactly() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 62, "unreal-engine-bin", "x86_64", 100).await;
        enqueue_pkg(&db, 63, "unreal", "x86_64", 200).await;

        // Neither neighbouring name is reserved.
        assert_eq!(claim(&db, 1).await.unwrap().id, 62);
        assert_eq!(claim(&db, 1).await.unwrap().id, 63);
    }

    /// An affine worker should take its affine job ahead of an older job that
    /// anybody could have taken — it may be the only worker that can.
    #[tokio::test]
    async fn affine_worker_prefers_its_affine_job() {
        let db = setup().await;
        worker(
            &db,
            W {
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 70, "hello", "x86_64", 100).await; // older, general
        enqueue_pkg(&db, 71, "unreal-engine", "x86_64", 200).await; // newer, affine

        assert_eq!(claim(&db, 1).await.unwrap().id, 71);
    }

    /// Liveness is deliberately not part of the affinity rule: handing the job
    /// elsewhere fails, where waiting merely waits.
    #[tokio::test]
    async fn offline_approved_affine_worker_still_reserves() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                last_seen: Some(now_secs() - 86_400),
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 72, "unreal-engine", "x86_64", 100).await;

        assert!(claim(&db, 1).await.is_none());
    }

    /// Revoking is the documented escape hatch for a reservation held by a
    /// machine that is never coming back.
    #[tokio::test]
    async fn revoked_affine_worker_does_not_reserve() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                status: ApprovalStatus::Revoked,
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 73, "unreal-engine", "x86_64", 100).await;

        assert_eq!(claim(&db, 1).await.unwrap().id, 73);
    }

    #[tokio::test]
    async fn affinity_and_arch_reservation_compose() {
        let db = setup().await;
        // Affine but wrong arch.
        worker(
            &db,
            W {
                id: 1,
                arches: "x86_64",
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        // Right arch but not affine.
        worker(
            &db,
            W {
                id: 2,
                arches: "aarch64",
                ..W::default()
            },
        )
        .await;
        // Both.
        worker(
            &db,
            W {
                id: 3,
                arches: "aarch64",
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 74, "unreal-engine", "aarch64", 100).await;

        assert!(claim(&db, 1).await.is_none(), "wrong arch claimed");
        assert!(claim(&db, 2).await.is_none(), "non-affine claimed");
        assert_eq!(claim(&db, 3).await.unwrap().id, 74);
    }

    // -------------------------------------------------------------- priority

    /// A fresh job, and a live idle faster worker exists: hold back.
    #[tokio::test]
    async fn lower_priority_worker_is_blocked_while_faster_one_is_available() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                priority: 10,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 80, "x86_64", now_secs()).await;

        assert!(claim(&db, 2).await.is_none(), "fallback jumped the queue");
        assert_eq!(claim(&db, 1).await.unwrap().id, 80);
    }

    /// Saturated fast worker: the fallback takes over immediately, with no
    /// delay — the case a fixed hold-back would have penalised.
    #[tokio::test]
    async fn spills_immediately_when_faster_worker_is_full() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                priority: 10,
                concurrency: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 81, "x86_64", now_secs()).await;
        enqueue(&db, 82, "x86_64", now_secs()).await;

        // Worker 1 fills its single slot...
        assert!(claim(&db, 1).await.is_some());
        // ...so worker 2 is free to take the other job right away.
        assert!(claim(&db, 2).await.is_some());
    }

    #[tokio::test]
    async fn spills_immediately_when_faster_worker_is_stale() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                priority: 10,
                last_seen: Some(now_secs() - 3600),
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 83, "x86_64", now_secs()).await;

        assert_eq!(claim(&db, 2).await.unwrap().id, 83);
    }

    /// The backstop: a faster worker that looks healthy but never claims must
    /// not stall a job forever.
    #[tokio::test]
    async fn spills_once_the_job_is_older_than_the_spill_delay() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                priority: 10,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 84, "x86_64", now_secs() - SPILL - 1).await;

        assert_eq!(claim(&db, 2).await.unwrap().id, 84);
    }

    #[tokio::test]
    async fn equal_priority_workers_do_not_block_each_other() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 85, "x86_64", now_secs()).await;

        assert_eq!(claim(&db, 2).await.unwrap().id, 85);
    }

    /// A faster worker that cannot build the job at all must not hold it back.
    #[tokio::test]
    async fn incapable_faster_worker_does_not_block() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                arches: "aarch64",
                priority: 10,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 86, "x86_64", now_secs()).await;

        assert_eq!(claim(&db, 2).await.unwrap().id, 86);
    }

    /// Priority must not override affinity: a faster worker without the
    /// affinity cannot take the job, and must not block the affine one either.
    #[tokio::test]
    async fn priority_does_not_override_affinity() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                priority: 10,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 87, "unreal-engine", "x86_64", now_secs()).await;

        assert!(claim(&db, 1).await.is_none());
        assert_eq!(claim(&db, 2).await.unwrap().id, 87);
    }

    // -------------------------------------------------- waiting reasons

    #[tokio::test]
    async fn no_reason_when_a_live_worker_could_take_the_job() {
        let db = setup().await;
        worker(&db, W::default()).await;
        enqueue(&db, 100, "x86_64", now_secs()).await;

        // Ordinary queueing is not a "reason" — otherwise every queued build
        // would carry an alarming explanation.
        assert!(waiting_reasons(&db, LIVENESS).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn explains_a_build_reserved_to_an_offline_worker() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                last_seen: Some(now_secs() - 86_400),
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 101, "unreal-engine", "x86_64", now_secs()).await;

        let reasons = waiting_reasons(&db, LIVENESS).await.unwrap();
        assert_eq!(
            reasons.get(&101),
            Some(&WaitingReason::Affinity {
                workers: vec!["w2".to_string()]
            })
        );
    }

    /// Closes a pre-existing blind spot: a foreign-arch build with no worker
    /// for that arch stalls silently today.
    #[tokio::test]
    async fn explains_an_arch_no_worker_can_build() {
        let db = setup().await;
        worker(&db, W::default()).await;
        enqueue(&db, 102, "aarch64", now_secs()).await;

        let reasons = waiting_reasons(&db, LIVENESS).await.unwrap();
        assert_eq!(
            reasons.get(&102),
            Some(&WaitingReason::Arch {
                arch: "aarch64".to_string()
            })
        );
    }

    #[tokio::test]
    async fn explains_a_fleet_that_is_entirely_offline() {
        let db = setup().await;
        worker(
            &db,
            W {
                last_seen: Some(now_secs() - 86_400),
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 103, "x86_64", now_secs()).await;

        let reasons = waiting_reasons(&db, LIVENESS).await.unwrap();
        assert_eq!(reasons.get(&103), Some(&WaitingReason::Offline));
    }

    // ------------------------------------------------------------- pause

    async fn pause(db: &DatabaseConnection, id: i32, paused: bool) {
        crate::helpers::worker_store::set_paused(db, id, paused)
            .await
            .unwrap()
            .expect("the worker exists");
    }

    /// A paused worker claims nothing, and takes work again once resumed.
    #[tokio::test]
    async fn a_paused_worker_claims_nothing_until_resumed() {
        let db = setup().await;
        worker(&db, W::default()).await;
        enqueue(&db, 200, "x86_64", now_secs()).await;

        pause(&db, 1, true).await;
        assert!(
            claim(&db, 1).await.is_none(),
            "a paused worker was handed a job"
        );
        pause(&db, 1, false).await;
        assert_eq!(claim(&db, 1).await.unwrap().id, 200);
    }

    /// Pausing the fast worker must not leave the fallback waiting for it: it
    /// will never take the job, so it does not count as available.
    #[tokio::test]
    async fn a_paused_worker_does_not_hold_back_a_lower_priority_one() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                priority: 10,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        pause(&db, 1, true).await;
        enqueue(&db, 201, "x86_64", now_secs()).await;

        assert_eq!(
            claim(&db, 2).await.map(|b| b.id),
            Some(201),
            "the fallback waited on a paused worker"
        );
    }

    /// A build only paused workers could take says so, naming them -- and a
    /// build someone else can take is just queued.
    #[tokio::test]
    async fn explains_a_build_only_paused_workers_could_take() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                affinity: "reserved",
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        pause(&db, 1, true).await;
        enqueue_pkg(&db, 202, "reserved", "x86_64", now_secs()).await;
        enqueue(&db, 203, "x86_64", now_secs()).await;

        let reasons = waiting_reasons(&db, LIVENESS).await.unwrap();
        assert_eq!(
            reasons.get(&202),
            Some(&WaitingReason::Paused {
                workers: vec!["w1".to_string()]
            })
        );
        assert_eq!(reasons.get(&203), None, "w2 can take it; that is a queue");
    }

    // ------------------------------------------------- leases and reaping

    #[tokio::test]
    async fn heartbeat_renews_reported_and_abandons_dropped() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 5,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 30, "x86_64", 100).await;
        enqueue(&db, 31, "x86_64", 100).await;
        claim_job(&db, 5, 60, SPILL, LIVENESS).await.unwrap();
        claim_job(&db, 5, 60, SPILL, LIVENESS).await.unwrap();

        // Report only build 30 as active -> 31 was dropped: failed, with a
        // fresh attempt in its place.
        let out = heartbeat(&db, 5, &[30], 60, 3).await.unwrap();
        assert_eq!(out.renewed, vec![30]);
        assert_eq!(
            out.dropped.iter().map(|a| a.build_id).collect::<Vec<_>>(),
            vec![31]
        );
        assert!(
            matches!(out.dropped[0].retry, Retry::Queued(_)),
            "a first loss is retried"
        );
        assert!(
            out.cancel_requested.is_empty(),
            "a reported, owned-ACTIVE build has nothing to cancel"
        );

        let b31 = Builds::find_by_id(31).one(&db).await.unwrap().unwrap();
        assert_eq!(b31.status, BuildState::Failed);
        assert_eq!(b31.end_reason, Some(EndReason::Dropped));
        // Kept: the record of who ran the attempt.
        assert_eq!(b31.worker_id, Some(5));
    }

    /// A build being published has no lease: its worker handed it over and no
    /// longer reports it. Nothing that polices leases may touch it -- not the
    /// heartbeat that no longer lists it, not the reaper, not a revocation --
    /// or a finished build would be requeued or failed while the server puts
    /// it in the repository.
    #[tokio::test]
    async fn a_build_being_published_is_out_of_reach_of_lease_policing() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 5,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 40, "x86_64", 100).await;
        claim_job(&db, 5, 60, SPILL, LIVENESS).await.unwrap();
        Builds::update_many()
            .col_expr(builds::Column::Status, BuildState::Publishing.into())
            .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into())
            .filter(builds::Column::Id.eq(40))
            .exec(&db)
            .await
            .unwrap();

        let out = heartbeat(&db, 5, &[], 60, 3).await.unwrap();
        assert!(out.dropped.is_empty(), "abandoned by a heartbeat");
        let reaped = reap_expired_builds(&db, now_secs() + 100_000, 3, 1)
            .await
            .unwrap();
        assert!(reaped.is_empty(), "abandoned by the reaper");
        assert!(
            abandon_worker_builds(&db, 5, 3).await.unwrap().is_empty(),
            "abandoned by a revocation"
        );

        let b = Builds::find_by_id(40).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, BuildState::Publishing);
        assert_eq!(b.worker_id, Some(5));
    }

    /// The abort list rides the same heartbeat: any id the worker reports that
    /// is missing, terminal, or owned by someone else must be flagged so the
    /// worker's next tick stops it. Owned-ACTIVE reported ids stay out.
    #[tokio::test]
    async fn heartbeat_flags_reported_but_lost_builds() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 5,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 6,
                ..W::default()
            },
        )
        .await;
        // 32: ours and ACTIVE -> renewed, not cancelled.
        enqueue(&db, 32, "x86_64", 100).await;
        claim_job(&db, 5, 60, SPILL, LIVENESS).await.unwrap();
        // 33: ours but terminal (the server abandoned it) -> cancel.
        enqueue(&db, 33, "x86_64", 100).await;
        claim_job(&db, 5, 60, SPILL, LIVENESS).await.unwrap();
        Builds::update_many()
            .col_expr(builds::Column::Status, BuildState::Failed.into())
            .filter(builds::Column::Id.eq(33))
            .exec(&db)
            .await
            .unwrap();
        // 34: reported, but no row (deleted / never existed) -> cancel.
        // 35: ACTIVE under the *other* worker -> cancel.
        enqueue(&db, 35, "x86_64", 100).await;
        claim_job(&db, 6, 60, SPILL, LIVENESS).await.unwrap();

        let out = heartbeat(&db, 5, &[32, 33, 34, 35], 60, 3).await.unwrap();
        assert_eq!(out.renewed, vec![32]);
        assert_eq!(out.cancel_requested, vec![33, 34, 35]);
        // 33's terminal state happened server-side; nothing else moved.
        assert!(out.dropped.is_empty());
    }

    #[tokio::test]
    async fn reaper_fails_expired_leases_and_queues_fresh_builds() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 3,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 60, "x86_64", 100).await;
        enqueue(&db, 61, "x86_64", 100).await;
        // Claim both with a 60s lease so lease_expires_at = claim_now + 60.
        claim_job(&db, 3, 60, SPILL, LIVENESS).await.unwrap();
        claim_job(&db, 3, 60, SPILL, LIVENESS).await.unwrap();

        // "now" far in the future: both leases are expired. Each abandoned row
        // is failed for good (naming the worker that ran it) and a *fresh*
        // build is queued in its place.
        let far = now_secs() + 10_000;
        let out = reap_expired_builds(&db, far, 3, 100_000).await.unwrap();
        assert!(failed(&out).is_empty());
        assert_eq!(retried(&out).len(), 2);

        for old_id in [60, 61] {
            let b = Builds::find_by_id(old_id).one(&db).await.unwrap().unwrap();
            assert_eq!(b.status, BuildState::Failed);
            assert_eq!(
                b.worker_id,
                Some(3),
                "the failed row must still name the worker that ran it"
            );
            assert_eq!(b.end_reason, Some(EndReason::LeaseExpired));
            assert_eq!(b.lease_expires_at, None);
            assert!(b.end_time.is_some());

            // A fresh ENQUEUED row, trigger timeout_retry, current version.
            let fresh = Builds::find()
                .filter(builds::Column::PkgId.eq(old_id))
                .filter(builds::Column::Id.ne(old_id))
                .one(&db)
                .await
                .unwrap()
                .expect("a fresh build must be queued in the abandoned row's place");
            assert_eq!(fresh.status, BuildState::Enqueued);
            assert_eq!(fresh.trigger, BuildTrigger::TimeoutRetry);
            assert!(fresh.worker_id.is_none());

            // Package repointed at it, moving off "Building".
            let pkg = Packages::find_by_id(old_id)
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(pkg.status, BuildState::Enqueued);
        }
    }

    /// A build that completes between the reaper's candidate select and its
    /// per-row write must not be dragged back out of its terminal state.
    #[tokio::test]
    async fn abandoning_skips_build_resolved_after_observation() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 7,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 90, "x86_64", 100).await;
        claim_job(&db, 7, 60, SPILL, LIVENESS).await.unwrap();

        // What the reaper would have seen in its candidate select.
        let observed = Builds::find_by_id(90).one(&db).await.unwrap().unwrap();

        // ...meanwhile the owning worker completes the build.
        Builds::update_many()
            .col_expr(builds::Column::Status, BuildState::Successful.into())
            .col_expr(builds::Column::WorkerId, Option::<i32>::None.into())
            .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into())
            .filter(builds::Column::Id.eq(90))
            .exec(&db)
            .await
            .unwrap();

        // The stale observation must not clobber the SUCCESS row.
        let outcome = abandon_build(&db, &observed, EndReason::LeaseExpired, 3)
            .await
            .unwrap();
        assert_eq!(outcome, None);

        let b = Builds::find_by_id(90).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, BuildState::Successful);
        assert_eq!(b.end_reason, None);
    }

    /// Same guard, but for a lease renewed by a heartbeat in the gap.
    #[tokio::test]
    async fn abandoning_skips_build_whose_lease_was_renewed() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 8,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 91, "x86_64", 100).await;
        claim_job(&db, 8, 60, SPILL, LIVENESS).await.unwrap();
        let observed = Builds::find_by_id(91).one(&db).await.unwrap().unwrap();

        // Worker heartbeats: lease pushed out, build still ACTIVE and owned.
        heartbeat(&db, 8, &[91], 600, 3).await.unwrap();

        let outcome = abandon_build(&db, &observed, EndReason::LeaseExpired, 3)
            .await
            .unwrap();
        assert_eq!(outcome, None);

        let b = Builds::find_by_id(91).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, BuildState::Active);
        assert_eq!(b.worker_id, Some(8));
    }

    #[tokio::test]
    async fn reaper_leaves_fresh_leases_alone() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 4,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 70, "x86_64", 100).await;
        claim_job(&db, 4, 600, SPILL, LIVENESS).await.unwrap();

        // "now" is right after the claim: lease is fresh, build is young.
        let out = reap_expired_builds(&db, now_secs(), 3, 100_000)
            .await
            .unwrap();
        assert!(retried(&out).is_empty());
        assert!(failed(&out).is_empty());
        let b = Builds::find_by_id(70).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, BuildState::Active);
    }

    #[tokio::test]
    async fn reaper_backstop_fires_despite_fresh_lease() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 6,
                ..W::default()
            },
        )
        .await;
        // start_time = 100 (long ago); claim gives a fresh far-future lease.
        enqueue(&db, 80, "x86_64", 100).await;
        claim_job(&db, 6, 1_000_000, SPILL, LIVENESS).await.unwrap();

        // Backstop: max_build_age small so start_time(=claim now) is "too old"
        // relative to a `now` well past it, even though the lease is fresh.
        let now = now_secs() + 10_000;
        let out = reap_expired_builds(&db, now, 3, 1).await.unwrap();
        assert_eq!(retried(&out).len(), 1, "80 must get a fresh attempt");
        let b = Builds::find_by_id(80).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, BuildState::Failed);
        assert_eq!(b.end_reason, Some(EndReason::MaxDuration));
    }

    /// The retry budget is derived from the history, not counted: three
    /// consecutive timeout_retry rows that all ended in abandonment spend it,
    /// and the package is left failed.
    #[tokio::test]
    async fn reaper_gives_up_after_budget_spent() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        // Package 90, with a package row of its own.
        db.execute_unprepared(
            "INSERT INTO packages (id, name, status, out_of_date, build_flags, platforms, \
                source_data, directly_requested) \
             VALUES (90, 'p90', 0, 0, '', 'x86_64', \
                '{\"type\":\"aur\",\"name\":\"p90\"}', 1)",
        )
        .await
        .unwrap();
        // Two prior futile runs the reaper already queued and abandoned.
        for (id, number) in [(88, 1), (89, 2)] {
            db.execute_unprepared(&format!(
                "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, \
                    number, trigger, end_reason, worker_id) \
                 VALUES ({id}, 90, {FAILED_BUILD}, 0, 'x86_64', '1.0', {number}, \
                    {t}, {reason}, 2)",
                t = BuildTrigger::TimeoutRetry.as_i32(),
                reason = EndReason::LeaseExpired.as_i32()
            ))
            .await
            .unwrap();
        }
        // The live build under test is the third futile attempt: a retry the last
        // abandonment queued (trigger timeout_retry), about to be abandoned.
        // start_time is recent so only the (expired) lease picks it up, never
        // the backstop deadline.
        db.execute_unprepared(&format!(
            "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, \
                number, trigger, worker_id, lease_expires_at) \
             VALUES (90, 90, {ACTIVE_BUILD}, {now}, 'x86_64', '1.0', 3, \
                {t}, 2, 0)",
            t = BuildTrigger::TimeoutRetry.as_i32(),
            now = now_secs()
        ))
        .await
        .unwrap();
        // Claiming pointed the package at the attempt it is running.

        let far = now_secs() + 10_000;
        let out = reap_expired_builds(&db, far, 3, 100_000).await.unwrap();
        assert_eq!(failed(&out), vec![90]);
        assert!(retried(&out).is_empty());

        let b = Builds::find_by_id(90).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, BuildState::Failed);
        assert_eq!(b.end_reason, Some(EndReason::LeaseExpired));
        assert_eq!(
            b.worker_id,
            Some(2),
            "even a budget-spent failure names the worker that ran it"
        );
        // No fourth build crossed the budget line, and the package is failed.
        assert_eq!(
            Builds::find()
                .filter(builds::Column::PkgId.eq(90))
                .count(&db)
                .await
                .unwrap(),
            3
        );
        let pkg = Packages::find_by_id(90).one(&db).await.unwrap().unwrap();
        assert_eq!(pkg.status, BuildState::Failed);
    }

    /// An abandonment is pinned to the lease revision the reaper observed: a
    /// heartbeat renewal landing in the read-write gap makes the CAS a no-op,
    /// so a healthy worker's live build is never terminally failed and nothing
    /// downstream runs.
    #[tokio::test]
    async fn abandon_skips_a_build_renewed_in_the_gap() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 8,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 91, "x86_64", 100).await;
        claim_job(&db, 8, 60, SPILL, LIVENESS).await.unwrap();
        let observed = Builds::find_by_id(91).one(&db).await.unwrap().unwrap();

        // Worker heartbeats between the reaper's select and its write: the
        // lease moves on, the observed pin no longer matches.
        heartbeat(&db, 8, &[91], 600, 3).await.unwrap();

        let outcome = abandon_build(&db, &observed, EndReason::LeaseExpired, 3)
            .await
            .unwrap();
        assert_eq!(outcome, None, "a renewed lease must make the CAS a no-op");

        let b = Builds::find_by_id(91).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, BuildState::Active);
        assert_eq!(b.worker_id, Some(8));
        assert_eq!(b.end_reason, None);
        // No fresh build was queued and the package was not touched.
        assert_eq!(
            Builds::find()
                .filter(builds::Column::PkgId.eq(91))
                .count(&db)
                .await
                .unwrap(),
            1
        );
        assert_eq!(pkg_status(&db, 91).await, BuildState::Active);
    }

    /// Seed package 95 with an abandoned `ACTIVE` build (#95, number 10,
    /// `timeout_retry`) and the given earlier rows `(id, number, platform)`,
    /// each a futile `timeout_retry` that ended in abandonment.
    async fn seed_abandonment(db: &DatabaseConnection, earlier: &[(i32, i32, &str)]) {
        worker(
            db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        db.execute_unprepared(
            "INSERT INTO packages (id, name, status, out_of_date, build_flags, platforms, \
                source_data, directly_requested) \
             VALUES (95, 'p95', 0, 0, '', 'x86_64;aarch64', \
                '{\"type\":\"aur\",\"name\":\"p95\"}', 1)",
        )
        .await
        .unwrap();
        for (id, number, platform) in earlier {
            db.execute_unprepared(&format!(
                "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, \
                    number, trigger, end_reason, worker_id) \
                 VALUES ({id}, 95, {FAILED_BUILD}, 0, '{platform}', '1.0', {number}, \
                    {t}, {reason}, 2)",
                t = BuildTrigger::TimeoutRetry.as_i32(),
                reason = EndReason::LeaseExpired.as_i32()
            ))
            .await
            .unwrap();
        }
        db.execute_unprepared(&format!(
            "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, \
                number, trigger, worker_id, lease_expires_at) \
             VALUES (95, 95, {ACTIVE_BUILD}, {now}, 'x86_64', '1.0', 10, {t}, 2, 0)",
            t = BuildTrigger::TimeoutRetry.as_i32(),
            now = now_secs()
        ))
        .await
        .unwrap();
    }

    /// Each platform has its own chain of retries. Another platform's
    /// abandonments interleaved in the history spend none of this one's budget.
    #[tokio::test]
    async fn another_platforms_abandonments_do_not_spend_the_budget() {
        let db = setup().await;
        seed_abandonment(
            &db,
            &[(91, 7, "aarch64"), (92, 8, "aarch64"), (93, 9, "aarch64")],
        )
        .await;

        let far = now_secs() + 10_000;
        let out = reap_expired_builds(&db, far, 3, 100_000).await.unwrap();

        assert!(
            failed(&out).is_empty(),
            "aarch64's retries spent x86_64's budget"
        );
        assert_eq!(retried(&out).len(), 1);
    }

    /// A package shows every platform it builds for: a spent budget on one
    /// fails the package even though another platform's newest build worked,
    /// and it would read as fine only once that one is retried successfully.
    #[tokio::test]
    async fn a_spent_budget_fails_the_package_whatever_its_other_platforms_did() {
        let db = setup().await;
        seed_abandonment(&db, &[(93, 8, "x86_64"), (94, 9, "x86_64")]).await;
        db.execute_unprepared(&format!(
            "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, number, trigger) \
             VALUES (96, 95, {SUCCESSFUL_BUILD}, 0, 'aarch64', '1.0', 11, {t})",
            t = BuildTrigger::User.as_i32()
        ))
        .await
        .unwrap();

        let far = now_secs() + 10_000;
        let out = reap_expired_builds(&db, far, 3, 100_000).await.unwrap();

        assert_eq!(failed(&out), vec![95], "the budget is spent");
        assert_eq!(pkg_status(&db, 95).await, BuildState::Failed);
    }

    /// The package's status is its builds' -- the newest per configured
    /// platform -- whatever path changed a build.
    #[test]
    fn the_most_active_state_leads_then_failure() {
        use crate::helpers::builds::combined_status;
        use BuildState::*;
        assert_eq!(combined_status([]), None);
        assert_eq!(combined_status([Successful, Enqueued]), Some(Enqueued));
        assert_eq!(combined_status([WaitingForDeps, Active]), Some(Active));
        assert_eq!(combined_status([Successful, Failed]), Some(Failed));
        assert_eq!(combined_status([Successful, Successful]), Some(Successful));
    }
}
