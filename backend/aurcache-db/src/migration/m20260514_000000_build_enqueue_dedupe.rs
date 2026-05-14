use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const ACTIVE_BUILD_STATUS: i32 = 0;
const FAILED_BUILD_STATUS: i32 = 2;
const ENQUEUED_BUILD_STATUS: i32 = 3;

async fn mark_duplicate_pending_builds_failed(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let db = manager.get_connection();
    let pending_filter = format!("status IN ({ACTIVE_BUILD_STATUS}, {ENQUEUED_BUILD_STATUS})");

    let update_sql = match database_type() {
        DbBackend::Sqlite => format!(
            r"
WITH ranked AS (
    SELECT
        id,
        ROW_NUMBER() OVER (
            PARTITION BY pkg_id, platform
            ORDER BY
                CASE WHEN status = {active} THEN 0 ELSE 1 END,
                COALESCE(start_time, 0) DESC,
                id DESC
        ) AS row_num
    FROM builds
    WHERE {pending_filter}
)
UPDATE builds
SET status = {failed}
WHERE id IN (SELECT id FROM ranked WHERE row_num > 1);
",
            active = ACTIVE_BUILD_STATUS,
            failed = FAILED_BUILD_STATUS,
            pending_filter = pending_filter,
        ),
        DbBackend::Postgres => format!(
            r"
WITH ranked AS (
    SELECT
        id,
        ROW_NUMBER() OVER (
            PARTITION BY pkg_id, platform
            ORDER BY
                CASE WHEN status = {active} THEN 0 ELSE 1 END,
                COALESCE(start_time, 0) DESC,
                id DESC
        ) AS row_num
    FROM public.builds
    WHERE {pending_filter}
)
UPDATE public.builds
SET status = {failed}
WHERE id IN (SELECT id FROM ranked WHERE row_num > 1);
",
            active = ACTIVE_BUILD_STATUS,
            failed = FAILED_BUILD_STATUS,
            pending_filter = pending_filter,
        ),
        _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
    };

    db.execute_unprepared(&update_sql).await?;
    Ok(())
}

async fn create_pending_build_unique_index(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let db = manager.get_connection();
    let index_sql = match database_type() {
        DbBackend::Sqlite => format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_builds_pending_pkg_platform ON builds (pkg_id, platform) WHERE status IN ({ACTIVE_BUILD_STATUS}, {ENQUEUED_BUILD_STATUS});"
        ),
        DbBackend::Postgres => format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_builds_pending_pkg_platform ON public.builds (pkg_id, platform) WHERE status IN ({ACTIVE_BUILD_STATUS}, {ENQUEUED_BUILD_STATUS});"
        ),
        _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
    };

    db.execute_unprepared(&index_sql).await?;
    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        mark_duplicate_pending_builds_failed(manager).await?;
        create_pending_build_unique_index(manager).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        match database_type() {
            DbBackend::Sqlite => {
                db.execute_unprepared("DROP INDEX IF EXISTS idx_builds_pending_pkg_platform;")
                    .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared("DROP INDEX IF EXISTS idx_builds_pending_pkg_platform;")
                    .await?;
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ActiveModelTrait, Database, DbErr, Set};
    use sea_orm_migration::MigratorTrait;

    use crate::builds;

    #[tokio::test]
    async fn duplicate_pending_builds_are_rejected() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        builds::ActiveModel {
            pkg_id: Set(1),
            output: Set(None),
            status: Set(Some(3)),
            start_time: Set(Some(1)),
            end_time: Set(None),
            platform: Set("x86_64".to_string()),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let duplicate = builds::ActiveModel {
            pkg_id: Set(1),
            output: Set(None),
            status: Set(Some(3)),
            start_time: Set(Some(2)),
            end_time: Set(None),
            platform: Set("x86_64".to_string()),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await;

        assert!(matches!(duplicate, Err(DbErr::Exec(_))));
    }
}
