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
                    r#"
alter table builds
add version TEXT not null default '';
"#,
                )
                .await?;

                // copy versions from package versions to new columns
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

                // drop the old packages version
                db.execute_unprepared(
                    r#"
alter table packages
drop column version;
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

                // drop the old packages version
                db.execute_unprepared(
                    r#"
ALTER TABLE packages
DROP COLUMN version;
"#,
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }

    // Reverse the migration: restore packages.version from builds.version and remove builds.version.
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match database_type() {
            DbBackend::Sqlite => {
                // add version column back to packages
                db.execute_unprepared(
                    r#"
alter table packages
add version TEXT not null default '';
"#,
                )
                .await?;

                // copy versions from builds back to packages
                db.execute_unprepared(
                    r#"
UPDATE packages
SET version = (
    SELECT COALESCE(builds.version, '')
    FROM builds
    WHERE builds.pkg_id = packages.id
);
"#,
                )
                .await?;

                // drop the version column from builds
                db.execute_unprepared(
                    r#"
alter table builds
drop column version;
"#,
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r#"
ALTER TABLE packages
ADD COLUMN version TEXT NOT NULL DEFAULT '';
"#,
                )
                .await?;

                db.execute_unprepared(
                    r#"
UPDATE packages
SET version = COALESCE(builds.version, '')
FROM builds
WHERE builds.pkg_id = packages.id;
"#,
                )
                .await?;

                // drop the version column from builds
                db.execute_unprepared(
                    r#"
ALTER TABLE builds
DROP COLUMN version;
"#,
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}
