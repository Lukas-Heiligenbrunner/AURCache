use crate::builds;
use crate::prelude::Builds;
use pacman_mirrors::platforms::Platform;
use sea_orm::sea_query::{OnConflict, Query};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DbErr, EntityTrait,
    IntoActiveModel, QueryFilter, TryIntoModel,
};

const ACTIVE_BUILD_STATUS: i32 = 0;
const ENQUEUED_BUILD_STATUS: i32 = 3;
const WAITING_FOR_DEPS_STATUS: i32 = 4;

pub struct EnqueueBuildResult {
    pub build: builds::Model,
    pub inserted: bool,
}

/// Insert a new pending build with the given `initial_status` if no pending build already exists
/// for `(pkg_id, platform)`.
///
/// `initial_status` must be one of `ENQUEUED_BUILD` or `WAITING_FOR_DEPS`.  The partial unique
/// index on `builds(pkg_id, platform)` covering all pending states (ACTIVE, ENQUEUED,
/// WAITING_FOR_DEPS) ensures at most one pending row per `(pkg_id, platform)` at any time.
///
/// If a pending build already exists the insert is skipped (`inserted = false`) and the existing
/// row is returned, regardless of its status.
pub async fn enqueue_build_if_missing<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
    version: &str,
    start_time: i64,
    initial_status: i32,
) -> Result<EnqueueBuildResult, DbErr> {
    let platform_str = platform.as_str();
    let insert = Query::insert()
        .into_table(builds::Entity)
        .columns([
            builds::Column::PkgId,
            builds::Column::Status,
            builds::Column::StartTime,
            builds::Column::Platform,
            builds::Column::Version,
        ])
        .values([
            pkg_id.into(),
            initial_status.into(),
            start_time.into(),
            platform_str.to_owned().into(),
            version.to_owned().into(),
        ])
        .map_err(|e| DbErr::Custom(e.to_string()))?
        .on_conflict(OnConflict::new().do_nothing().to_owned())
        .to_owned();

    let result = db.execute(db.get_database_backend().build(&insert)).await?;

    let build = Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform_str))
        .filter(builds::Column::Status.is_in(vec![
            Some(ACTIVE_BUILD_STATUS),
            Some(ENQUEUED_BUILD_STATUS),
            Some(WAITING_FOR_DEPS_STATUS),
        ]))
        .one(db)
        .await?
        .ok_or_else(|| {
            DbErr::Custom(format!(
                "Missing pending build row for package {pkg_id} on platform {platform_str}"
            ))
        })?;

    Ok(EnqueueBuildResult {
        build,
        inserted: result.rows_affected() == 1,
    })
}

/// Promote an existing `WAITING_FOR_DEPS` build to `ENQUEUED` so that it can be started.
///
/// Returns the updated build row if a `WAITING_FOR_DEPS` build was found and promoted,
/// or `None` if no such build exists (e.g. the build was never queued or was already promoted).
pub async fn promote_waiting_build<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
) -> Result<Option<builds::Model>, DbErr> {
    let Some(build) = Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform.as_str()))
        .filter(builds::Column::Status.eq(Some(WAITING_FOR_DEPS_STATUS)))
        .one(db)
        .await?
    else {
        return Ok(None);
    };

    let mut active = build.into_active_model();
    active.status = Set(Some(ENQUEUED_BUILD_STATUS));
    let updated = active.save(db).await?.try_into_model()?;
    Ok(Some(updated))
}
