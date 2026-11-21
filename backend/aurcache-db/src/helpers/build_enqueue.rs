use crate::helpers::builds::refresh_package_status;
use crate::prelude::{Builds, Packages};
use crate::{builds, packages};
use aurcache_common::build_state::{BuildState, BuildTrigger};
use pacman_mirrors::platforms::Platform;
use sea_orm::TransactionSession;
use sea_orm::sea_query::{Expr, ExprTrait, Func, OnConflict, Query};
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QuerySelect};

/// The pending build for a `(package, platform)`, and how it came to be.
pub struct EnqueueBuildResult {
    pub build: builds::Model,
    pub queued: Queued,
}

/// How a call left the pending build it returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Queued {
    /// Inserted by this call.
    Inserted,
    /// Moved from `WAITING_FOR_DEPS` to `ENQUEUED` by this call.
    Promoted,
    /// Already pending, and left as it was.
    Existing,
}

/// The states a new build can be queued in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pending {
    /// Ready for a worker to claim.
    Enqueued,
    /// Held until what it depends on is built.
    WaitingForDeps,
}

impl Pending {
    /// The state the row is written with.
    #[must_use]
    pub const fn state(self) -> BuildState {
        match self {
            Self::Enqueued => BuildState::Enqueued,
            Self::WaitingForDeps => BuildState::WaitingForDeps,
        }
    }
}

// See the race explanation in `enqueue_build_if_missing`.
const NUMBER_RACE_ATTEMPTS: usize = 5;

/// `(SELECT COALESCE(MAX(number), 0) + 1 FROM builds WHERE pkg_id = <pkg_id>)`.
///
/// Assembled through the query builder rather than written as a raw fragment:
/// sea-query substitutes a raw `?` only for backends whose placeholder *is*
/// `?`, so Postgres would get the marker verbatim and fail with `syntax error
/// at or near ")"`. The tests all run on SQLite, so the one below renders both
/// backends.
fn next_build_number_expr(pkg_id: i32) -> Expr {
    Expr::from(
        Query::select()
            .expr(Func::coalesce([Expr::col(builds::Column::Number).max(), Expr::val(0)]).add(1))
            .from(builds::Entity)
            .and_where(Expr::col(builds::Column::PkgId).eq(pkg_id))
            .to_owned(),
    )
}

/// Insert a new pending build, queued as `pending`, if no pending build already exists
/// for `(pkg_id, platform)`; the package's status follows.
///
/// The partial unique index on `builds(pkg_id, platform)` covering every state
/// in [`BuildState::IN_PROGRESS`] ensures at most one pending row per
/// `(pkg_id, platform)` at any time.
///
/// If a pending build already exists the insert is skipped ([`Queued::Existing`]) and the
/// existing row is returned, regardless of its status.
///
/// A package that is only ever uploaded ([`packages::uploaded`]) is
/// refused: no worker could build it. Every queueing path comes through here,
/// so this holds for one that forgot to ask, too.
pub async fn enqueue_build_if_missing<C: ConnectionTrait + sea_orm::TransactionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
    version: &str,
    start_time: i64,
    pending: Pending,
    // What the row is for. The retry budget for abandoned builds reads this
    // column, so every creation path has to say.
    trigger: BuildTrigger,
) -> Result<EnqueueBuildResult, DbErr> {
    if is_uploaded(db, pkg_id).await? {
        return Err(DbErr::Custom(format!(
            "package {pkg_id} is built elsewhere and uploaded; no build of it can be queued"
        )));
    }
    let txn = crate::helpers::txn::begin_write(db).await?;
    // Two conflicts can stop this insert, and they mean opposite things.
    //
    // `(pkg_id, platform)` means a pending build already exists — the intended
    // outcome, and the row we then return.
    //
    // `(pkg_id, number)` means another insert for the same package took this
    // number between our subquery and our write. Nothing is wrong with *this*
    // build; it simply needs the next number. Without the retry that race
    // surfaces as "Missing pending build row", because `DO NOTHING` swallowed
    // an insert that should have happened.
    for _ in 0..NUMBER_RACE_ATTEMPTS {
        // Read inside the statement rather than in a separate query, so the
        // window a competing insert can land in is as small as the database
        // allows.
        let next_number = next_build_number_expr(pkg_id);

        let insert = Query::insert()
            .into_table(builds::Entity)
            .columns([
                builds::Column::PkgId,
                builds::Column::Status,
                builds::Column::StartTime,
                builds::Column::Platform,
                builds::Column::Version,
                builds::Column::Number,
                builds::Column::Trigger,
            ])
            .values([
                pkg_id.into(),
                pending.state().into(),
                start_time.into(),
                platform.into(),
                version.to_owned().into(),
                next_number,
                trigger.as_i32().into(),
            ])
            .map_err(|e| DbErr::Custom(e.to_string()))?
            .on_conflict(OnConflict::new().do_nothing().to_owned())
            .to_owned();

        let result = txn.execute(&insert).await?;

        let existing = crate::helpers::builds::pending_build(&txn, pkg_id, platform).await?;

        if let Some(build) = existing {
            let queued = if result.rows_affected() == 1 {
                refresh_package_status(&txn, pkg_id).await?;
                Queued::Inserted
            } else {
                Queued::Existing
            };
            txn.commit().await?;
            return Ok(EnqueueBuildResult { build, queued });
        }
        // Nothing inserted and nothing pending: the number was taken. Try again
        // with a freshly read maximum.
    }

    Err(DbErr::Custom(format!(
        "Could not assign a build number for package {pkg_id} on platform {platform} \
         after {NUMBER_RACE_ATTEMPTS} attempts"
    )))
}

/// Whether package `pkg_id` is only ever uploaded ([`packages::uploaded`]).
async fn is_uploaded<C: ConnectionTrait>(db: &C, pkg_id: i32) -> Result<bool, DbErr> {
    let stored: Option<Option<String>> = Packages::find_by_id(pkg_id)
        .select_only()
        .column(packages::Column::SourceData)
        .into_tuple()
        .one(db)
        .await?;
    Ok(packages::uploaded(stored.flatten().as_deref()))
}

/// What [`record_upload`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadSlot {
    /// The upload's build, inserted straight into `PUBLISHING`.
    Recorded(builds::Model),
    /// Nothing inserted: this build of the package on that platform has not
    /// settled yet, and an upload does not go past it.
    Busy(builds::Model),
}

/// Insert the build an upload becomes: already `PUBLISHING`, since everything
/// a worker would do for it is done, with no worker and no lease. The
/// package's status follows, in `txn`.
///
/// The partial unique index over [`BuildState::IN_PROGRESS`] is what refuses
/// a second one, as it refuses a second queued build: [`UploadSlot::Busy`]
/// names the build in the way.
pub async fn record_upload<C: ConnectionTrait>(
    txn: &C,
    pkg_id: i32,
    platform: Platform,
    version: &str,
    now: i64,
) -> Result<UploadSlot, DbErr> {
    // The same two conflicts as in `enqueue_build_if_missing`, told apart the
    // same way.
    for _ in 0..NUMBER_RACE_ATTEMPTS {
        if let Some(pending) = crate::helpers::builds::pending_build(txn, pkg_id, platform).await? {
            return Ok(UploadSlot::Busy(pending));
        }
        let insert = Query::insert()
            .into_table(builds::Entity)
            .columns([
                builds::Column::PkgId,
                builds::Column::Status,
                builds::Column::StartTime,
                builds::Column::Platform,
                builds::Column::Version,
                builds::Column::Number,
                builds::Column::Trigger,
            ])
            .values([
                pkg_id.into(),
                BuildState::Publishing.into(),
                now.into(),
                platform.into(),
                version.to_owned().into(),
                next_build_number_expr(pkg_id),
                BuildTrigger::Upload.as_i32().into(),
            ])
            .map_err(|e| DbErr::Custom(e.to_string()))?
            .on_conflict(OnConflict::new().do_nothing().to_owned())
            .to_owned();
        if txn.execute(&insert).await?.rows_affected() == 1 {
            let build = crate::helpers::builds::pending_build(txn, pkg_id, platform)
                .await?
                .ok_or_else(|| DbErr::Custom("the uploaded build vanished".to_string()))?;
            refresh_package_status(txn, pkg_id).await?;
            return Ok(UploadSlot::Recorded(build));
        }
    }
    Err(DbErr::Custom(format!(
        "Could not assign a build number for package {pkg_id} on platform {platform} \
         after {NUMBER_RACE_ATTEMPTS} attempts"
    )))
}

/// Promote an existing `WAITING_FOR_DEPS` build to `ENQUEUED` so that it can be started.
///
/// Returns the updated build row if a `WAITING_FOR_DEPS` build was found and promoted,
/// or `None` if no such build exists (e.g. the build was never queued or was already promoted).
///
/// The write is pinned to `status = WAITING_FOR_DEPS`, like
/// [`demote_enqueued_build`]: a cancel landing between the read and the write
/// would otherwise be overwritten, putting a cancelled build back in the queue.
pub async fn promote_waiting_build<C: ConnectionTrait + sea_orm::TransactionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
) -> Result<Option<builds::Model>, DbErr> {
    let txn = crate::helpers::txn::begin_write(db).await?;
    let Some(build) = Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform.as_str()))
        .filter(builds::Column::Status.eq(BuildState::WaitingForDeps))
        .one(&txn)
        .await?
    else {
        return Ok(None);
    };

    let res = Builds::update_many()
        .col_expr(builds::Column::Status, Expr::value(BuildState::Enqueued))
        .filter(builds::Column::Id.eq(build.id))
        .filter(builds::Column::Status.eq(BuildState::WaitingForDeps))
        .exec(&txn)
        .await?;
    if res.rows_affected == 0 {
        return Ok(None);
    }
    refresh_package_status(&txn, pkg_id).await?;
    txn.commit().await?;
    Ok(Some(builds::Model {
        status: BuildState::Enqueued,
        ..build
    }))
}

/// Put an `ENQUEUED` build back to `WAITING_FOR_DEPS`, because something it
/// needs is no longer ready.
///
/// The counterpart of [`promote_waiting_build`], and the reason a package's
/// dependencies can be repointed while a build for it is already queued: the
/// queue entry was made against the old dependency, and the new one may not be
/// built yet.
///
/// A conditional `UPDATE` rather than the read-then-write `promote_waiting_build`
/// does, because the race runs the other way here. A queued build is exactly
/// what a worker is looking for, so one can be claimed (`ENQUEUED -> ACTIVE`)
/// between a read and a write -- and demoting it then would take a build a
/// worker is already running and put it back in the queue. Pinning the update to
/// `status = ENQUEUED` makes that a no-op instead.
pub async fn demote_enqueued_build<C: ConnectionTrait + sea_orm::TransactionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
) -> Result<Demotion, DbErr> {
    let txn = crate::helpers::txn::begin_write(db).await?;
    let res = Builds::update_many()
        .col_expr(
            builds::Column::Status,
            Expr::value(BuildState::WaitingForDeps),
        )
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform.as_str()))
        .filter(builds::Column::Status.eq(BuildState::Enqueued))
        .exec(&txn)
        .await?;
    if res.rows_affected == 0 {
        return Ok(Demotion::Unchanged);
    }
    refresh_package_status(&txn, pkg_id).await?;
    txn.commit().await?;
    Ok(Demotion::Demoted)
}

/// What [`demote_enqueued_build`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demotion {
    /// The queued build is back to waiting for its dependencies.
    Demoted,
    /// There was no queued build: none existed, or a worker claimed it first.
    Unchanged,
}

#[cfg(test)]
mod build_number_tests {
    use super::next_build_number_expr;
    use sea_orm::DatabaseBackend;
    use sea_orm::sea_query::Query;

    /// Every supported backend has to bind the package id. A backend that
    /// renders a bare `?` is emitting a placeholder the driver will not fill.
    #[test]
    fn next_build_number_binds_the_package_id_on_every_backend() {
        let select = Query::select().expr(next_build_number_expr(7)).to_owned();

        for backend in [DatabaseBackend::Sqlite, DatabaseBackend::Postgres] {
            let sql = backend.build(&select).to_string();
            assert!(
                sql.contains('7'),
                "{backend:?} dropped the package id: {sql}"
            );
            assert!(
                !sql.contains('?'),
                "{backend:?} left an unsubstituted placeholder: {sql}"
            );
        }
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;

    async fn uploaded_package(db: &sea_orm::DatabaseConnection) {
        db.execute_unprepared(
            "INSERT INTO packages (id, name, platforms, source_data) \
             VALUES (1, 'hello', 'x86_64', '{\"type\":\"upload\"}')",
        )
        .await
        .unwrap();
    }

    /// An uploaded source that also carries an `archive` field is just as
    /// uploaded.
    #[tokio::test]
    async fn an_uploaded_package_in_any_spelling_is_never_queued() {
        let db = crate::testing::memory_db().await;
        db.execute_unprepared(
            "INSERT INTO packages (id, name, platforms, source_data) \
             VALUES (1, 'hello', 'x86_64', '{\"type\":\"upload\",\"archive\":[]}')",
        )
        .await
        .unwrap();
        assert!(
            enqueue_build_if_missing(
                &db,
                1,
                Platform::X86_64,
                "1-1",
                100,
                Pending::Enqueued,
                BuildTrigger::User
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn an_uploaded_package_is_never_queued() {
        let db = crate::testing::memory_db().await;
        uploaded_package(&db).await;
        for pending in [Pending::Enqueued, Pending::WaitingForDeps] {
            assert!(
                enqueue_build_if_missing(
                    &db,
                    1,
                    Platform::X86_64,
                    "1-1",
                    100,
                    pending,
                    BuildTrigger::User
                )
                .await
                .is_err()
            );
        }
        assert!(Builds::find().one(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_upload_is_recorded_publishing_and_a_second_waits() {
        let db = crate::testing::memory_db().await;
        uploaded_package(&db).await;
        let UploadSlot::Recorded(build) = record_upload(&db, 1, Platform::X86_64, "1-1", 100)
            .await
            .unwrap()
        else {
            panic!("the first upload is recorded");
        };
        assert_eq!(build.status, BuildState::Publishing);
        assert_eq!(build.trigger, BuildTrigger::Upload);
        assert_eq!(build.number, 1);
        assert_eq!(build.worker_id, None);
        let status: BuildState = Packages::find_by_id(1)
            .select_only()
            .column(packages::Column::Status)
            .into_tuple()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, BuildState::Publishing);

        assert_eq!(
            record_upload(&db, 1, Platform::X86_64, "1-2", 101)
                .await
                .unwrap(),
            UploadSlot::Busy(build)
        );
        // Another platform is another slot.
        assert!(matches!(
            record_upload(&db, 1, Platform::Aarch64, "1-1", 102)
                .await
                .unwrap(),
            UploadSlot::Recorded(_)
        ));
    }

    #[tokio::test]
    async fn a_failed_package_refresh_rolls_back_each_queue_mutation() {
        let db = crate::testing::memory_db().await;
        db.execute_unprepared(
            "INSERT INTO packages (id, name, platforms) VALUES (1, 'hello', 'x86_64')",
        )
        .await
        .unwrap();
        db.execute_unprepared("CREATE TRIGGER refuse_status BEFORE UPDATE OF status ON packages BEGIN SELECT RAISE(ABORT, 'status failure'); END").await.unwrap();
        assert!(
            enqueue_build_if_missing(
                &db,
                1,
                Platform::X86_64,
                "1-1",
                100,
                Pending::WaitingForDeps,
                BuildTrigger::User
            )
            .await
            .is_err()
        );
        assert!(Builds::find().one(&db).await.unwrap().is_none());
        db.execute_unprepared("DROP TRIGGER refuse_status")
            .await
            .unwrap();
        let build = enqueue_build_if_missing(
            &db,
            1,
            Platform::X86_64,
            "1-1",
            100,
            Pending::WaitingForDeps,
            BuildTrigger::User,
        )
        .await
        .unwrap()
        .build;
        db.execute_unprepared("CREATE TRIGGER refuse_status BEFORE UPDATE OF status ON packages BEGIN SELECT RAISE(ABORT, 'status failure'); END").await.unwrap();
        assert!(
            promote_waiting_build(&db, 1, Platform::X86_64)
                .await
                .is_err()
        );
        assert_eq!(
            Builds::find_by_id(build.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .status,
            BuildState::WaitingForDeps
        );
        db.execute_unprepared("DROP TRIGGER refuse_status")
            .await
            .unwrap();
        promote_waiting_build(&db, 1, Platform::X86_64)
            .await
            .unwrap();
        db.execute_unprepared("CREATE TRIGGER refuse_status BEFORE UPDATE OF status ON packages BEGIN SELECT RAISE(ABORT, 'status failure'); END").await.unwrap();
        assert!(
            demote_enqueued_build(&db, 1, Platform::X86_64)
                .await
                .is_err()
        );
        assert_eq!(
            Builds::find_by_id(build.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .status,
            BuildState::Enqueued
        );
    }
}
