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
CREATE TABLE settings
(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    key TEXT NOT NULL,
    value TEXT,
    pkg_id INTEGER NOT NULL, -- always NOT NULL now
    -- should actually be foreign key -> but drops errors if null
    UNIQUE (pkg_id, key)     -- full UNIQUE constraint
);
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
CREATE TABLE public.settings
(
    id SERIAL PRIMARY KEY,
    key TEXT NOT NULL,
    value TEXT,
    pkg_id INTEGER NOT NULL,               -- global settings use -1
    CONSTRAINT settings_packages_fk        -- optional foreign key, can point to packages table
        FOREIGN KEY (pkg_id)
        REFERENCES packages(id),
    CONSTRAINT settings_unique_per_pkg     -- full UNIQUE constraint on (pkg_id, key)
        UNIQUE (pkg_id, key)
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
