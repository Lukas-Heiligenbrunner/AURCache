use crate::builds;
use crate::prelude::Builds;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait, QueryFilter, QueryOrder, Statement,
};

const ACTIVE_BUILD_STATUS: i32 = 0;
const ENQUEUED_BUILD_STATUS: i32 = 3;

pub struct EnqueueBuildResult {
    pub build: builds::Model,
    pub inserted: bool,
}

pub async fn enqueue_build_if_missing<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: &str,
    version: &str,
    start_time: i64,
) -> Result<EnqueueBuildResult, DbErr> {
    // This helper relies on the partial unique index created by
    // m20260514_000000_build_enqueue_dedupe to guarantee there is at most one
    // pending build row per `(pkg_id, platform)` across ACTIVE/ENQUEUED states.
    let (sql, values) = match db.get_database_backend() {
        DbBackend::Sqlite => (
            "INSERT INTO builds (pkg_id, status, start_time, platform, version)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT DO NOTHING;",
            vec![
                pkg_id.into(),
                ENQUEUED_BUILD_STATUS.into(),
                start_time.into(),
                platform.into(),
                version.into(),
            ],
        ),
        DbBackend::Postgres => (
            "INSERT INTO public.builds (pkg_id, status, start_time, platform, version)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT DO NOTHING;",
            vec![
                pkg_id.into(),
                ENQUEUED_BUILD_STATUS.into(),
                start_time.into(),
                platform.into(),
                version.into(),
            ],
        ),
        _ => return Err(DbErr::Custom("Unsupported database backend".to_string())),
    };

    let result = db
        .execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            sql,
            values,
        ))
        .await?;

    let build = Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform))
        .filter(
            builds::Column::Status
                .is_in(vec![Some(ACTIVE_BUILD_STATUS), Some(ENQUEUED_BUILD_STATUS)]),
        )
        .order_by_desc(builds::Column::Id)
        .one(db)
        .await?
        .ok_or_else(|| {
            DbErr::Custom(format!(
                "Missing pending build row for package {pkg_id} on platform {platform}"
            ))
        })?;

    Ok(EnqueueBuildResult {
        build,
        inserted: result.rows_affected() == 1,
    })
}
