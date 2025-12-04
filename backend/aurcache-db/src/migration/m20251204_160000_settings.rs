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
                // add new column version to builds
                db.execute_unprepared(
                    r"
create table settings
(
	id integer not null
		constraint settings_pk
			primary key autoincrement,
    key    TEXT not null,
    value  TEXT,
    pkg_id integer
        constraint settings_packages_id_fk
            references packages
);
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
create table public.settings
(
    id SERIAL PRIMARY KEY
    key    TEXT not null,
    value  TEXT,
    pkg_id integer
        constraint settings_packages_id_fk
            references packages
);
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
                // add version column back to packages
                db.execute_unprepared(
                    r"
drop table settings;
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
drop table settings;
",
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}
