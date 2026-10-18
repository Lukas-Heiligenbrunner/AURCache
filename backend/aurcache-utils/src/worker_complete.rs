//! Server-side handling of a worker's build completion: terminal status
//! transitions, version reconciliation, and promotion of dependent builds.
//!
//! Workers poll for enqueued jobs, so promoting a dependent from
//! `WAITING_FOR_DEPS` to `ENQUEUED` is all that is required to dispatch it.

use crate::build_logger::append_build_output;
use crate::publish::publish_with_recovery;
use crate::repository::Repository;
use crate::worker_policy::worker_policy;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::api::log::{BuildRef, WorkerRef};
use aurcache_common::build_state::BuildState;
use aurcache_common::build_state::EndReason;
use aurcache_common::worker::{BuildOutcome, CompleteReport};
use aurcache_db::helpers::build_enqueue::{Demotion, demote_enqueued_build, promote_waiting_build};
use aurcache_db::helpers::builds::refresh_package_status;
use aurcache_db::helpers::time::now_secs;
use aurcache_db::helpers::worker_jobs;
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use aurcache_db::{builds, dependencies};
use pacman_mirrors::platforms::Platform;
use sea_orm::DatabaseConnection;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QuerySelect, TransactionSession,
    TransactionTrait,
};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug)]
pub enum CompletionError {
    NotFound(i32),
    LeaseLost { build_id: i32, worker_id: i32 },
    Persistence(DbErr),
}
impl std::fmt::Display for CompletionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "build {id} not found"),
            Self::LeaseLost {
                build_id,
                worker_id,
            } => write!(
                f,
                "build {build_id} is no longer owned+active by worker {worker_id} (lease lost)"
            ),
            Self::Persistence(error) => std::fmt::Display::fmt(error, f),
        }
    }
}
impl std::error::Error for CompletionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Persistence(error) => Some(error),
            _ => None,
        }
    }
}
impl From<DbErr> for CompletionError {
    fn from(error: DbErr) -> Self {
        Self::Persistence(error)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Completion {
    Accepted,
    AlreadyAccepted,
}

/// The build `build_id`, or an error naming it.
async fn find_build<C: ConnectionTrait>(
    db: &C,
    build_id: i32,
) -> Result<builds::Model, CompletionError> {
    Builds::find_by_id(build_id)
        .one(db)
        .await?
        .ok_or_else(|| CompletionError::NotFound(build_id))
}

/// Confirm the given worker currently holds the active lease on the build.
/// Uploads and completions are only accepted from the owning worker while the
/// build is `ACTIVE`.
pub async fn assert_owned_active<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
    build_id: i32,
) -> Result<builds::Model, CompletionError> {
    let build = find_build(db, build_id).await?;
    check_owned_active(worker_id, &build)?;
    Ok(build)
}

/// As [`assert_owned_active`], without requiring the build still be `ACTIVE`.
///
/// For reports that are not a claim on the build's state, only a comment
/// about it: a worker's problem report can arrive just after the build's
/// terminal state lands (cleanup, an upload still finishing), and discarding
/// it there would lose exactly the report an operator most wants to see.
pub async fn assert_owned<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
    build_id: i32,
) -> Result<builds::Model, CompletionError> {
    let build = find_build(db, build_id).await?;
    check_owned(worker_id, &build)?;
    Ok(build)
}

/// The ownership half of [`assert_owned_active`], for callers that already
/// hold the row: completions fetch the build first for their idempotency
/// checks, and re-querying it here would pay a second point lookup on the
/// hottest worker endpoint for a row that cannot usefully change between the
/// two reads (a concurrent state change fails the CAS further down anyway).
pub fn check_owned_active(worker_id: i32, build: &builds::Model) -> Result<(), CompletionError> {
    if build.status != BuildState::Active {
        return Err(lease_lost(build.id, worker_id));
    }
    check_owned(worker_id, build)
}

/// Whether `worker_id` is the worker that ran `build`, as an error when not.
fn check_owned(worker_id: i32, build: &builds::Model) -> Result<(), CompletionError> {
    if build.worker_id != Some(worker_id) {
        return Err(lease_lost(build.id, worker_id));
    }
    Ok(())
}

/// Record the peak memory a worker reported for a build.
///
/// Written before the success/failure branch, because a build that was
/// OOM-killed is exactly when the number is worth having and that build will
/// never reach the success path.
///
/// Best-effort rather than compare-and-swap: unlike the version and the size,
/// this publishes nothing and decides nothing, so a build whose lease was
/// reclaimed mid-flight is not worth failing a completion over. A stale write
/// records a measurement that did happen, for a build that did run.
pub async fn record_peak_memory<C: ConnectionTrait>(
    db: &C,
    build_id: i32,
    peak_memory_bytes: i64,
) -> Result<(), DbErr> {
    Builds::update_many()
        .col_expr(builds::Column::PeakMemory, Some(peak_memory_bytes).into())
        .filter(builds::Column::Id.eq(build_id))
        .exec(db)
        .await?;
    Ok(())
}

/// Record the disk a build used on its worker, part by part. Best-effort, for
/// the same reason as [`record_peak_memory`], and for a failed build too: one
/// that ran out of disk is the one whose figures matter most.
pub async fn record_disk_usage<C: ConnectionTrait>(
    db: &C,
    build_id: i32,
    usage: &aurcache_common::api::builds::DiskUsage,
) -> Result<(), DbErr> {
    Builds::update_many()
        .col_expr(builds::Column::DiskChroot, usage.chroot.into())
        .col_expr(builds::Column::DiskWorkdir, usage.workdir.into())
        .col_expr(builds::Column::DiskSources, usage.sources.into())
        .col_expr(builds::Column::DiskBuildTree, usage.build_tree.into())
        .filter(builds::Column::Id.eq(build_id))
        .exec(db)
        .await?;
    Ok(())
}

/// Record the failed chroot the worker kept for inspection, where and until
/// when. Best-effort, for the same reason as [`record_peak_memory`]: a stale
/// write records a keep that did happen, for a build that did run.
pub async fn record_kept<C: ConnectionTrait>(
    db: &C,
    build_id: i32,
    kept: &aurcache_common::api::builds::KeptBuild,
) -> Result<(), DbErr> {
    Builds::update_many()
        .col_expr(builds::Column::KeptPath, Some(kept.path.clone()).into())
        .col_expr(builds::Column::KeptUntil, Some(kept.until).into())
        .col_expr(builds::Column::KeptTree, kept.tree.clone().into())
        .filter(builds::Column::Id.eq(build_id))
        .exec(db)
        .await?;
    Ok(())
}

/// A build the worker was building is no longer `ACTIVE`-and-owned by it (the
/// lease was reclaimed by the reaper and possibly re-handed to another worker).
/// The late completion must be discarded rather than clobbering the new owner.
fn lease_lost(build_id: i32, worker_id: i32) -> CompletionError {
    CompletionError::LeaseLost {
        build_id,
        worker_id,
    }
}

/// End `worker_id`'s lease on `build_id`, moving the build from `ACTIVE` to
/// `status` -- and its package's status with it -- in one transaction;
/// `ended_at` is the build's end time when it is over.
///
/// A compare-and-swap on `status = ACTIVE AND worker_id = ?`, so a completion
/// arriving after the reaper reclaimed the build is discarded rather than
/// clobbering the new owner.
///
/// `worker_id` is kept. On a build that has left `ACTIVE` it is no longer a
/// claim, it is the record of which machine produced it -- which the workers
/// page promises. Nothing mistakes it for ownership: every path that treats a
/// build as owned -- heartbeat, `abandon_worker_builds`, the lease reaper,
/// `claim_job` -- also requires `ACTIVE`.
async fn release<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    build_id: i32,
    worker_id: i32,
    status: BuildState,
    ended_at: Option<i64>,
) -> Result<(), CompletionError> {
    let build = find_build(db, build_id).await?;

    let txn = db.begin().await?;
    let mut update = Builds::update_many()
        .col_expr(builds::Column::Status, status.into())
        .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into());
    if let Some(ended_at) = ended_at {
        update = update.col_expr(builds::Column::EndTime, Some(ended_at).into());
    }
    let res = update
        .filter(builds::Column::Id.eq(build_id))
        .filter(builds::Column::Status.eq(BuildState::Active))
        .filter(builds::Column::WorkerId.eq(worker_id))
        .exec(&txn)
        .await?;
    if res.rows_affected == 0 {
        txn.rollback().await?;
        return Err(lease_lost(build_id, worker_id));
    }

    refresh_package_status(&txn, build.pkg_id).await?;
    txn.commit().await?;
    Ok(())
}

/// Take a finished build over from the worker that built it, to publish.
///
/// The worker's artifacts are all uploaded and it has said so: from here on
/// nothing it could do would change the outcome, so it is released. The build
/// moves from `ACTIVE` to `PUBLISHING` -- which is also what takes it out of
/// the reach of the heartbeat, the reaper and a revocation, all of which only
/// look at `ACTIVE` builds.
pub async fn accept_for_publishing<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    build_id: i32,
    worker_id: i32,
) -> Result<(), CompletionError> {
    release(db, build_id, worker_id, BuildState::Publishing, None).await
}

/// Mark a build (and its package) as failed. Deterministic failures reported by
/// the worker are terminal — no re-enqueue.
pub async fn complete_failure<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    build_id: i32,
    worker_id: i32,
) -> Result<(), CompletionError> {
    release(
        db,
        build_id,
        worker_id,
        BuildState::Failed,
        Some(now_secs()),
    )
    .await
}

/// After a successful build, promote any dependent whose dependencies are now
/// all satisfied from `WAITING_FOR_DEPS` to `ENQUEUED` so a worker can claim it.
///
/// Returns the builds it promoted, for the caller to log.
pub async fn trigger_dependents<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
) -> Result<Vec<BuildRef>, DbErr> {
    let deps_by_dependent = load_dependencies_for_dependents_of(db, pkg_id).await?;
    let mut promoted = vec![];
    for (dependent_id, all_deps) in &deps_by_dependent {
        if aurcache_db::helpers::builds::dependencies_satisfied(db, all_deps, platform).await?
            && let Some(build) = promote_dependent(db, *dependent_id, platform).await?
        {
            promoted.push(build);
        }
    }
    Ok(promoted)
}

/// Re-decide whether one package's queued build can still start, after its
/// dependencies changed.
///
/// Repointing or dropping a dependency edge changes the answer in *both*
/// directions, which is why this is not just [`trigger_dependents`]:
///
/// * a build held at `WAITING_FOR_DEPS` may now be free to go, because the
///   dependency that was holding it up is no longer one of this package's --
///   this is what makes "replace the dependency" unblock a build rather than
///   leave it waiting on a package it no longer needs;
/// * a build already `ENQUEUED` may now have to wait, because the replacement
///   it was pointed at has not been built yet. Leaving it queued sends it to a
///   worker that cannot resolve its dependencies, and the build fails for a
///   reason the queue already knew about.
///
/// Every platform the package has a pending build on, since a dependency may be
/// satisfied on one and not another. `ACTIVE` builds are left alone: they are
/// running, and the queue has nothing left to say about them.
///
/// Returns the ids of the builds it let start, for the caller to log.
pub async fn resync_pending_builds<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    pkg_id: i32,
) -> Result<Vec<i32>, DbErr> {
    let pending: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Status.is_in([BuildState::Enqueued, BuildState::WaitingForDeps]))
        .all(db)
        .await?;
    if pending.is_empty() {
        return Ok(vec![]);
    }
    let mut unblocked = vec![];

    let deps = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(pkg_id))
        .all(db)
        .await?;

    for build in pending {
        let platform = build.platform;
        let ready =
            aurcache_db::helpers::builds::dependencies_satisfied(db, &deps, platform).await?;
        match (build.status, ready) {
            (BuildState::WaitingForDeps, true) => {
                if let Some(promoted) = promote_waiting_build(db, pkg_id, platform).await? {
                    tracing::info!(
                        "Build #{} on {platform} can start: its dependencies changed and are now satisfied",
                        promoted.id
                    );
                    unblocked.push(promoted.id);
                }
            }
            (BuildState::Enqueued, false)
                if demote_enqueued_build(db, pkg_id, platform).await? == Demotion::Demoted =>
            {
                tracing::info!(
                    "Build #{} on {platform} must wait: its dependencies changed and are not satisfied yet",
                    build.id
                );
            }
            _ => {}
        }
    }
    Ok(unblocked)
}

async fn load_dependencies_for_dependents_of<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
) -> Result<HashMap<i32, Vec<dependencies::Model>>, DbErr> {
    let dependent_ids: Vec<i32> = Dependencies::find()
        .filter(dependencies::Column::DependeeId.eq(pkg_id))
        .select_only()
        .column(dependencies::Column::DependentId)
        .into_tuple()
        .all(db)
        .await?;
    if dependent_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let deps = Dependencies::find()
        .filter(dependencies::Column::DependentId.is_in(dependent_ids))
        .all(db)
        .await?;
    let mut map: HashMap<i32, Vec<dependencies::Model>> = HashMap::new();
    for dep in deps {
        map.entry(dep.dependent_id).or_default().push(dep);
    }
    Ok(map)
}

async fn promote_dependent<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    dependent_id: i32,
    platform: Platform,
) -> Result<Option<BuildRef>, DbErr> {
    let Some(pkg) = Packages::find_by_id(dependent_id).one(db).await? else {
        return Ok(None);
    };
    if !pkg.platforms.as_slice().is_empty() && !pkg.platforms.contains(platform) {
        return Ok(None);
    }
    Ok(promote_waiting_build(db, pkg.id, platform)
        .await?
        .map(|promoted| BuildRef {
            pkgbase: pkg.name,
            number: promoted.number,
        }))
}

/// Accept a completion, including retries, publication and staging cleanup.
pub async fn complete(
    db: &DatabaseConnection,
    repo: &Arc<Repository>,
    activity: &ActivityLog,
    worker_model: &aurcache_db::workers::Model,
    build_id: i32,
    report: CompleteReport,
) -> Result<Completion, CompletionError> {
    let build = find_build(db, build_id).await?;

    // A report about a build this worker ran that has already moved on is a
    // repeat, and is answered 200 without changing anything: the worker's
    // earlier request got through and only its answer was lost.
    //
    // * A cancelled report acknowledges an abort the server carried out
    //   (operator Stop, or the reaper abandoning the build).
    // * A success repeats a completion that was accepted: the build is being
    //   published, was published, or failed to publish -- which fails with no
    //   end reason, unlike a cancel or an abandonment.
    //
    // A real ownership check: without it, any authenticated worker that
    // guessed an id could remove another worker's staging directory.
    if build.worker_id == Some(worker_model.id) {
        let failed = build.status == BuildState::Failed;
        // An interruption the server already abandoned the build for is the
        // same: the build left this worker's hands either way.
        let acknowledged_abort = matches!(
            report.outcome,
            BuildOutcome::Canceled | BuildOutcome::Interrupted
        ) && failed;
        let repeated_success = report.outcome == BuildOutcome::Succeeded
            && (matches!(
                build.status,
                BuildState::Publishing | BuildState::Successful
            ) || (failed && build.end_reason.is_none()));
        if acknowledged_abort {
            let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
            return Ok(Completion::AlreadyAccepted);
        }
        if repeated_success {
            return Ok(Completion::AlreadyAccepted);
        }
    }

    check_owned_active(worker_model.id, &build)?;

    // What the entries below name it by. A package gone from under an active
    // build is not something to refuse the report over.
    let pkgbase = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|pkg| pkg.name);
    let named = pkgbase.as_ref().map(|pkgbase| BuildRef {
        pkgbase: pkgbase.clone(),
        number: build.number,
    });
    let worker = || WorkerRef::from(worker_model.name.as_str());
    let record_failed = |what: &str, error: String| {
        if let Some(build) = &named {
            activity.emit(Event::BuildRecordFailed {
                build: build.clone(),
                what: what.to_string(),
                error,
            });
        }
    };

    // Before the branch below: an OOM-killed build never reaches the success
    // path, and that is the build whose memory figure matters most.
    if let Some(peak) = report.peak_memory_bytes
        && let Err(e) = record_peak_memory(db, build_id, peak).await
    {
        record_failed("peak memory", e.to_string());
    }
    if let Some(usage) = report.disk_usage.as_ref().filter(|u| !u.is_empty())
        && let Err(e) = record_disk_usage(db, build_id, usage).await
    {
        record_failed("disk usage", e.to_string());
    }
    if let Some(kept) = report.kept.as_ref()
        && let Err(e) = record_kept(db, build_id, kept).await
    {
        record_failed("kept chroot", e.to_string());
    }

    if report.outcome == BuildOutcome::Succeeded {
        // What the worker actually checked out replaces what the server
        // guessed when it queued the build. Only from a success, and only when
        // the worker reported something: one that could not resolve a source
        // leaves the queue-time record standing -- the older commit, so the
        // error is a redundant rebuild rather than a missed one.
        if !report.vcs_commits.is_empty()
            && let Err(e) = aurcache_db::helpers::builds::record_build_vcs_sources(
                db,
                build_id,
                &report.vcs_commits,
            )
            .await
        {
            record_failed("built VCS sources", e.to_string());
        }
        accept_for_publishing(db, build_id, worker_model.id).await?;
        if let Some(build) = named.clone() {
            activity.emit(Event::BuildSucceeded {
                build,
                worker: worker(),
            });
        }
        let (db, repo, activity) = (db.clone(), Arc::clone(repo), activity.clone());
        tokio::spawn(async move {
            publish_with_recovery(&db, &repo, &activity, build_id).await;
        });
        return Ok(Completion::Accepted);
    }

    if let Some(reason) = &report.reason
        && let Some(pkgbase) = &pkgbase
        && let Err(e) = append_build_output(
            pkgbase,
            build.number,
            &format!("worker reported failure: {reason}\n"),
        )
        .await
    {
        record_failed("failure reason", e.to_string());
    }

    if report.outcome == BuildOutcome::Interrupted {
        let abandoned = worker_jobs::abandon_build(
            db,
            &build,
            EndReason::Interrupted,
            worker_policy().max_attempts,
        )
        .await?
        .ok_or_else(|| lease_lost(build_id, worker_model.id))?;
        crate::abandoned::report(db, activity, &[abandoned]).await;
        let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
        return Ok(Completion::Accepted);
    }
    // Refused like a success above when the lease was lost in the meantime.
    complete_failure(db, build_id, worker_model.id).await?;
    if let Some(build) = named {
        activity.emit(Event::BuildFailed {
            build,
            worker: worker(),
            reason: report.reason.clone(),
        });
    }
    let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
    Ok(Completion::Accepted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_db::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    async fn pkg(db: &DatabaseConnection, id: i32) {
        db.execute_unprepared(&format!(
            "INSERT INTO packages \
             (id, name, status, out_of_date, build_flags, platforms, source_data, directly_requested) \
             VALUES ({id}, 'p{id}', 0, 0, '', 'x86_64', '{{\"type\":\"aur\",\"name\":\"p{id}\"}}', 1)"
        ))
        .await
        .unwrap();
    }

    async fn build(
        db: &DatabaseConnection,
        id: i32,
        pkg_id: i32,
        status: BuildState,
        worker: &str,
    ) {
        db.execute_unprepared(&format!(
            "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, worker_id) \
             VALUES ({id}, {pkg_id}, {}, 0, 'x86_64', '1.0', {worker})",
            status.as_i32()
        ))
        .await
        .unwrap();
    }

    /// A finished build has to keep saying which machine produced it. The
    /// workers page tells operators the row is kept "so old builds still name
    /// the machine that ran them", and every per-worker figure -- how many
    /// builds it has finished, how many failed, its share of the fleet -- is
    /// counted from this column. Clearing it on completion left every one of
    /// them at zero for a fleet that had built everything in the repository.
    /// The figure has to survive to the row, and it must land for a *failed*
    /// build too: an OOM kill is the case it exists for, and that build never
    /// reaches the success path.
    #[tokio::test]
    async fn peak_memory_is_recorded_for_a_failed_build() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 1, 1, BuildState::Active, "1").await;

        record_peak_memory(&db, 1, 6 * 1024 * 1024 * 1024)
            .await
            .unwrap();
        complete_failure(&db, 1, 1).await.unwrap();

        let row = Builds::find_by_id(1).one(&db).await.unwrap().unwrap();
        assert_eq!(row.peak_memory, Some(6 * 1024 * 1024 * 1024));
        assert_eq!(row.status, BuildState::Failed);
    }

    /// A build's disk usage is kept part by part, and a part the worker did
    /// not measure stays unknown rather than becoming 0.
    #[tokio::test]
    async fn disk_usage_is_recorded_part_by_part() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 1, 1, BuildState::Active, "1").await;

        let usage = aurcache_common::api::builds::DiskUsage {
            chroot: Some(700),
            workdir: Some(50),
            sources: Some(9),
            build_tree: None,
        };
        record_disk_usage(&db, 1, &usage).await.unwrap();

        let row = Builds::find_by_id(1).one(&db).await.unwrap().unwrap();
        assert_eq!(row.disk_chroot, Some(700));
        assert_eq!(row.disk_workdir, Some(50));
        assert_eq!(row.disk_sources, Some(9));
        assert_eq!(row.disk_build_tree, None, "not measured is not zero");
    }

    /// A kept failure records where its chroot is and until when, for the
    /// build's page to point an operator at.
    #[tokio::test]
    async fn a_kept_failure_records_where_and_until_when() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 1, 1, BuildState::Active, "1").await;

        let kept = aurcache_common::api::builds::KeptBuild {
            path: "/pool/kept-1".to_string(),
            until: 1_800_000_000,
            tree: Some("/pool/kept-1.build".to_string()),
        };
        record_kept(&db, 1, &kept).await.unwrap();
        complete_failure(&db, 1, 1).await.unwrap();

        let row = Builds::find_by_id(1).one(&db).await.unwrap().unwrap();
        assert_eq!(row.kept_path.as_deref(), Some("/pool/kept-1"));
        assert_eq!(row.kept_until, Some(1_800_000_000));
        assert_eq!(row.kept_tree.as_deref(), Some("/pool/kept-1.build"));
        assert_eq!(row.status, BuildState::Failed);
    }

    #[tokio::test]
    async fn a_finished_build_still_names_the_worker_that_ran_it() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, BuildState::Active, "5").await;

        accept_for_publishing(&db, 10, 5).await.unwrap();

        let finished = Builds::find_by_id(10).one(&db).await.unwrap().unwrap();
        assert_eq!(finished.status, BuildState::Publishing);
        assert_eq!(
            finished.worker_id,
            Some(5),
            "the build no longer names the worker that produced it"
        );
        // The lease is a claim on a running build and does end here.
        assert_eq!(finished.lease_expires_at, None);
    }

    async fn dep(db: &DatabaseConnection, dependent: i32, dependee: i32) {
        db.execute_unprepared(&format!(
            "INSERT INTO dependencies (dependent_id, dependee_id, version_constraint) \
             VALUES ({dependent}, {dependee}, '')"
        ))
        .await
        .unwrap();
    }

    async fn status_of(db: &DatabaseConnection, build_id: i32) -> BuildState {
        Builds::find_by_id(build_id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    /// The point of repointing a dependency: the build waiting on the old one
    /// can go, because the old one is no longer what it needs.
    #[tokio::test]
    async fn a_waiting_build_starts_once_its_dependency_is_repointed() {
        let db = setup().await;
        pkg(&db, 1).await; // the dependent
        pkg(&db, 2).await; // the replacement, already built
        build(&db, 20, 2, BuildState::Successful, "1").await;
        build(&db, 10, 1, BuildState::WaitingForDeps, "NULL").await;
        dep(&db, 1, 2).await;

        resync_pending_builds(&db, 1).await.unwrap();

        assert_eq!(status_of(&db, 10).await, BuildState::Enqueued);
    }

    /// A promotion is an all-of, not a check of the edge that just changed.
    ///
    /// Repointing one dependency says nothing about the others: the build is
    /// only free to start when *every* dependency is satisfied, and promoting on
    /// the strength of the one that was touched would send a build to a worker
    /// that still cannot resolve the rest.
    #[tokio::test]
    async fn a_waiting_build_stays_waiting_while_another_dependency_is_unbuilt() {
        let db = setup().await;
        pkg(&db, 1).await; // the dependent
        pkg(&db, 2).await; // the repointed dependency, built
        pkg(&db, 3).await; // a second dependency, never built
        build(&db, 20, 2, BuildState::Successful, "1").await;
        build(&db, 10, 1, BuildState::WaitingForDeps, "NULL").await;
        dep(&db, 1, 2).await;
        dep(&db, 1, 3).await;

        resync_pending_builds(&db, 1).await.unwrap();

        assert_eq!(
            status_of(&db, 10).await,
            BuildState::WaitingForDeps,
            "promoted on one satisfied dependency while another is unbuilt"
        );

        // ... and it goes as soon as the last one lands.
        build(&db, 30, 3, BuildState::Successful, "1").await;
        resync_pending_builds(&db, 1).await.unwrap();
        assert_eq!(status_of(&db, 10).await, BuildState::Enqueued);
    }

    /// And the other direction, which is the one a promotion-only pass misses:
    /// a build already queued against the old dependency has to wait when the
    /// replacement has not been built.
    #[tokio::test]
    async fn a_queued_build_waits_when_its_new_dependency_is_not_built() {
        let db = setup().await;
        pkg(&db, 1).await;
        pkg(&db, 2).await; // the replacement, never built
        build(&db, 10, 1, BuildState::Enqueued, "NULL").await;
        dep(&db, 1, 2).await;

        resync_pending_builds(&db, 1).await.unwrap();

        assert_eq!(status_of(&db, 10).await, BuildState::WaitingForDeps);
    }

    /// A build a worker is already running is none of the queue's business.
    #[tokio::test]
    async fn a_running_build_is_left_alone() {
        let db = setup().await;
        pkg(&db, 1).await;
        pkg(&db, 2).await;
        build(&db, 10, 1, BuildState::Active, "5").await;
        dep(&db, 1, 2).await;

        resync_pending_builds(&db, 1).await.unwrap();

        assert_eq!(status_of(&db, 10).await, BuildState::Active);
    }

    /// Dropping the last dependency leaves nothing to wait for.
    #[tokio::test]
    async fn a_waiting_build_starts_when_its_last_dependency_is_dropped() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, BuildState::WaitingForDeps, "NULL").await;

        resync_pending_builds(&db, 1).await.unwrap();

        assert_eq!(status_of(&db, 10).await, BuildState::Enqueued);
    }

    #[tokio::test]
    async fn owned_active_guard() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, BuildState::Active, "5").await;
        assert!(assert_owned_active(&db, 5, 10).await.is_ok());
        assert!(matches!(
            assert_owned_active(&db, 6, 10).await,
            Err(CompletionError::LeaseLost {
                build_id: 10,
                worker_id: 6
            })
        ));
        assert!(matches!(
            assert_owned_active(&db, 5, 99).await,
            Err(CompletionError::NotFound(99))
        ));
        db.clone().close().await.unwrap();
        assert!(matches!(
            assert_owned_active(&db, 5, 10).await,
            Err(CompletionError::Persistence(_))
        ));
    }

    /// Accepting a completion releases the worker: the build is the server's to
    /// publish, with no lease left for anything to police, and nothing recorded
    /// as finished until it is published.
    #[tokio::test]
    async fn an_accepted_build_is_published_without_its_worker() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, BuildState::Active, "5").await;
        db.execute_unprepared("UPDATE builds SET lease_expires_at = 999 WHERE id = 10")
            .await
            .unwrap();

        accept_for_publishing(&db, 10, 5).await.unwrap();

        let b = Builds::find_by_id(10).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, BuildState::Publishing);
        assert_eq!(b.lease_expires_at, None);
        assert_eq!(b.worker_id, Some(5));
        assert_eq!(b.end_time, None, "not finished until published");
        let p = Packages::find_by_id(1).one(&db).await.unwrap().unwrap();
        assert_eq!(p.status, BuildState::Publishing);

        // Accepted once: a repeat is no longer an ACTIVE build of this worker.
        assert!(accept_for_publishing(&db, 10, 5).await.is_err());
    }

    #[tokio::test]
    async fn failure_is_terminal_not_requeued() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, BuildState::Active, "5").await;
        complete_failure(&db, 10, 5).await.unwrap();
        let b = Builds::find_by_id(10).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, BuildState::Failed);
        // A failed build produced nothing to measure. `None` says that; a `0`
        // would claim it produced an empty package.
        assert_eq!(b.size, None);
    }

    #[tokio::test]
    async fn completion_by_non_owner_is_discarded() {
        let db = setup().await;
        pkg(&db, 1).await;
        // Build reclaimed and re-handed to worker 6.
        build(&db, 10, 1, BuildState::Active, "6").await;
        // Late completion from the original owner (worker 5) must not clobber it.
        assert!(accept_for_publishing(&db, 10, 5).await.is_err());
        assert!(complete_failure(&db, 10, 5).await.is_err());
        let b = Builds::find_by_id(10).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, BuildState::Active);
        assert_eq!(b.worker_id, Some(6));
    }
}
