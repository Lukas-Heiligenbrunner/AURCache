use crate::helpers::dbtype::database_type;
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
                    r"
UPDATE packages
SET build_flags = REPLACE(build_flags, '-S', '-B')
WHERE build_flags LIKE '%-S%';
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
UPDATE public.packages
SET build_flags = REGEXP_REPLACE(build_flags, '-S', '-B', 'g')
WHERE build_flags ~ '-S';
",
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
                    r"
UPDATE packages
SET build_flags = REPLACE(build_flags, '-B', '-S')
WHERE build_flags LIKE '%-B%';
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
UPDATE public.packages
SET build_flags = REGEXP_REPLACE(build_flags, '-B', '-S', 'g')
WHERE build_flags ~ '-B';
",
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}
