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
create table activity
(
	id integer not null
		constraint activity_pk
			primary key autoincrement,
	typ integer not null,
	data TEXT not null,
	timestamp INTEGER not null,
	user TEXT
);
"#,
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r#"
CREATE TABLE public.activity (
    id SERIAL PRIMARY KEY,
    typ INTEGER NOT NULL,
    data TEXT NOT NULL,
    timestamp BIGINT NOT NULL,
    "user" TEXT
);
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
                db.execute_unprepared(r"DROP TABLE IF EXISTS activity;")
                    .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(r"DROP TABLE IF EXISTS activity;")
                    .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}
