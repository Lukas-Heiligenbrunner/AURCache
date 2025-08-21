use crate::db::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match database_type() {
            DbBackend::Sqlite => {
                db.execute_unprepared(
                    r#"
ALTER TABLE packages
ADD COLUMN package_type INTEGER DEFAULT 0 NOT NULL;

ALTER TABLE packages
ADD COLUMN custom_pkgbuild_path TEXT;
"#,
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r#"
ALTER TABLE public.packages
ADD COLUMN package_type INTEGER DEFAULT 0 NOT NULL;

ALTER TABLE public.packages
ADD COLUMN custom_pkgbuild_path TEXT;
"#,
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match database_type() {
            DbBackend::Sqlite => {
                db.execute_unprepared(
                    r#"
ALTER TABLE packages
DROP COLUMN package_type;

ALTER TABLE packages
DROP COLUMN custom_pkgbuild_path;
"#,
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r#"
ALTER TABLE public.packages
DROP COLUMN package_type;

ALTER TABLE public.packages
DROP COLUMN custom_pkgbuild_path;
"#,
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}