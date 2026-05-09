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
                    "ALTER TABLE files ADD COLUMN package_id INTEGER;",
                )
                .await?;

                db.execute_unprepared(
                    "UPDATE files SET package_id = (SELECT package_id FROM packages_files WHERE packages_files.file_id = files.id LIMIT 1);",
                )
                .await?;

                db.execute_unprepared(
                    "DROP TABLE IF EXISTS packages_files;",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    "ALTER TABLE public.files ADD COLUMN package_id INTEGER;",
                )
                .await?;

                db.execute_unprepared(
                    "UPDATE public.files SET package_id = (SELECT package_id FROM public.packages_files WHERE packages_files.file_id = files.id LIMIT 1);",
                )
                .await?;

                db.execute_unprepared(
                    "ALTER TABLE public.files ALTER COLUMN package_id SET NOT NULL;",
                )
                .await?;

                db.execute_unprepared(
                    "DROP TABLE IF EXISTS public.packages_files;",
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
                    "ALTER TABLE files DROP COLUMN package_id;",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    "ALTER TABLE public.files DROP COLUMN package_id;",
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}
