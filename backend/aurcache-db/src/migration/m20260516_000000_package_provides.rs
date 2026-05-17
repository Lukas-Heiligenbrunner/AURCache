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
                db.execute_unprepared("ALTER TABLE packages ADD COLUMN provides TEXT;")
                    .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared("ALTER TABLE public.packages ADD COLUMN provides TEXT;")
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
                db.execute_unprepared("ALTER TABLE packages DROP COLUMN provides;")
                    .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared("ALTER TABLE public.packages DROP COLUMN provides;")
                    .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}
