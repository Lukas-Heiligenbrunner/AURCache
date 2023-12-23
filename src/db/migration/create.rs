use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Use `execute_unprepared` if the SQL statement doesn't have value bindings
        db.execute_unprepared(
            r#"
create table builds
(
	id integer not null
		constraint builds_pk
			primary key autoincrement,
	pkg_id integer not null,
	version_id integer not null,
	ouput TEXT,
	status integer
);

create table packages
(
	id integer not null
		primary key autoincrement,
	name text not null
);

create table status
(
	id integer not null
		constraint status_pk
			primary key autoincrement,
	value TEXT
);

create table versions
(
	id integer not null
		constraint versions_pk
			primary key autoincrement,
	version TEXT not null,
	package_id integer not null,
	file_name TEXT,
	status INTEGER
);
            "#,
        )
        .await?;
        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
