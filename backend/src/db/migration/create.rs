use crate::db::helpers::dbtype::{database_type, DbType};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match database_type() {
            DbType::SQLITE => {
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
	filename TEXT not null,
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
		constraint packages_builds_id_fk
			references builds
);

create table packages_files
(
	file_id integer not null
		constraint packages_files_files_id_fk
			references files,
	package_id integer not null
		constraint packages_files_packages_id_fk
			references packages
	id integer not null
		constraint packages_files_pk
			primary key autoincrement
);

"#,
                )
                .await?;
            }
            DbType::POSTGRES => {
                db.execute_unprepared(
                    r#"
CREATE SCHEMA IF NOT EXISTS public;

CREATE TABLE public.builds (
    id SERIAL PRIMARY KEY,
    pkg_id INTEGER NOT NULL,
    version_id INTEGER NOT NULL,
    output TEXT,
    status INTEGER,
    start_time OID,
    end_time OID
);

CREATE TABLE public.versions (
    id SERIAL PRIMARY KEY,
    version TEXT NOT NULL,
    package_id INTEGER NOT NULL,
    file_name TEXT
);

CREATE TABLE public.packages (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    status INTEGER DEFAULT 0 NOT NULL,
    out_of_date INTEGER DEFAULT 0 NOT NULL,
    latest_version_id INTEGER REFERENCES public.versions(id),
    latest_aur_version TEXT
);
"#,
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        // this is the initial db schema state, so no down script here!
        Ok(())
    }
}
