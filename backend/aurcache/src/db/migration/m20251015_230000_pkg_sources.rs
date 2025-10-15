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
alter table packages
    add source_type TEXT default 'aur' not null;

alter table packages
    add source_data TEXT;

"#,
                )
                    .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r#"
alter table packages
    add source_type TEXT default 'aur' not null;

alter table packages
    add source_data TEXT;

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
DROP COLUMN source_type;

ALTER TABLE packages
DROP COLUMN source_data;
"#,
                )
                    .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r#"
ALTER TABLE packages
DROP COLUMN source_type;

ALTER TABLE packages
DROP COLUMN source_data;
"#,
                )
                    .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}
