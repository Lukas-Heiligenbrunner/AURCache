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
alter table builds
add version TEXT not null default '';
"#,
                )
                .await?;

                db.execute_unprepared(
                    r#"
UPDATE builds
SET version = (
    SELECT COALESCE(packages.version, '')
    FROM packages
    WHERE packages.id = builds.pkg_id
);
"#,
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r#"
ALTER TABLE builds
ADD COLUMN version TEXT NOT NULL DEFAULT '';
"#,
                )
                .await?;

                db.execute_unprepared(
                    r#"
UPDATE builds
SET version = COALESCE(packages.version, '')
FROM packages
WHERE packages.id = builds.pkg_id;
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
create table builds_dg_tmp
(
    id         integer not null
        constraint builds_pk
            primary key autoincrement,
    pkg_id     integer not null,
    output     TEXT,
    status     integer,
    start_time INTEGER,
    end_time   integer,
    platform   TEXT
);

insert into builds_dg_tmp(id, pkg_id, output, status, start_time, end_time, platform)
select id, pkg_id, output, status, start_time, end_time, platform
from builds;

drop table builds;

alter table builds_dg_tmp
    rename to builds;
"#,
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r#"
ALTER TABLE builds DROP COLUMN IF EXISTS version;
"#,
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}
