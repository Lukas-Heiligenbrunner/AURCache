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
create table builds
(
	id integer not null
		constraint builds_pk
			primary key autoincrement,
	pkg_id integer not null,
	output TEXT,
	status integer,
	start_time INTEGER,
	end_time integer
);

create table files
(
	filename TEXT not null
		constraint files_pk_2
			unique,
	id integer not null
		constraint files_pk
			primary key autoincrement
);

create table packages
(
	id integer not null
		constraint packages_pk
			primary key autoincrement,
	name text not null,
	status integer default 0 not null,
	out_of_date INTEGER default 0 not null,
	version TEXT not null,
	latest_aur_version TEXT,
	latest_build integer
);

create table packages_files
(
	file_id integer not null,
	package_id integer not null,
	id integer not null
		constraint packages_files_pk
			primary key autoincrement
);
"#,
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r#"
CREATE SCHEMA IF NOT EXISTS public;

CREATE TABLE public.builds (
    id SERIAL PRIMARY KEY,
    pkg_id INTEGER NOT NULL,
    output TEXT,
    status INTEGER,
    start_time BIGINT,
    end_time BIGINT
);

CREATE TABLE public.files (
    filename TEXT NOT NULL UNIQUE,
    id SERIAL PRIMARY KEY
);

CREATE TABLE public.packages (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    status INTEGER DEFAULT 0 NOT NULL,
    out_of_date INTEGER DEFAULT 0 NOT NULL,
    latest_build INTEGER,
    latest_aur_version TEXT,
    version TEXT NOT NULL
);

CREATE TABLE public.packages_files
(
    file_id INTEGER NOT NULL,
    package_id INTEGER NOT NULL,
    id SERIAL PRIMARY KEY
);
"#,
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        // this is the initial db schema state, so no down script here!
        Ok(())
    }
}
