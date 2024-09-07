use crate::db::helpers::dbtype::{database_type, DbType};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match database_type() {
            DbType::Sqlite => {
                db.execute_unprepared(
                    r#"
ALTER TABLE packages
ADD COLUMN build_flags TEXT;

ALTER TABLE packages
ADD COLUMN platforms TEXT;

ALTER TABLE builds
ADD COLUMN platform TEXT;

UPDATE packages
SET build_flags = '-Syu;--noconfirm;--noprogressbar;--color never',
    platforms = 'amd64';

UPDATE builds
    SET platform = 'amd64';
"#,
                )
                .await?;
            }
            DbType::Postgres => {
                db.execute_unprepared(
                    r#"
ALTER TABLE public.packages
ADD COLUMN build_flags TEXT;

ALTER TABLE public.packages
ADD COLUMN platforms TEXT;

ALTER TABLE public.builds
ADD COLUMN platform TEXT;

UPDATE public.packages
SET build_flags = '-Syu;--noconfirm;--noprogressbar;--color never',
    platforms = 'amd64';

UPDATE public.builds
    SET platform = 'amd64';
"#,
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match database_type() {
            DbType::Sqlite => {
                db.execute_unprepared(
                    r#"
ALTER TABLE packages
DROP COLUMN build_flags;

ALTER TABLE packages
DROP COLUMN platforms;

ALTER TABLE builds
DROP COLUMN platform;
"#,
                )
                .await?;
            }
            DbType::Postgres => {
                db.execute_unprepared(
                    r#"
ALTER TABLE public.packages
DROP COLUMN build_flags;

ALTER TABLE public.packages
DROP COLUMN platforms;

ALTER TABLE builds
DROP COLUMN platform;
"#,
                )
                .await?;
            }
        }

        Ok(())
    }
}
