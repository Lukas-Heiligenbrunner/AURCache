//! One file name per platform, not per repository.
//!
//! 0.5.0 made `files.filename` unique, and it came through the upgrade that
//! way. Every query already reads files per platform, but the constraint did
//! not: an `arch=any` package built for two platforms produces the same file
//! name in each platform's directory, so the second could never be published
//! ("duplicate key value violates unique constraint files_filename_key").

use sea_orm::{ConnectionTrait, DbBackend};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// SQLite cannot change a table's constraints in place, so the table is
/// rebuilt, columns named one by one, and its index made again.
const SQLITE_UP: &str = "
CREATE TABLE files_new (
    filename TEXT NOT NULL,
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    platform TEXT,
    package_id INTEGER NOT NULL REFERENCES packages (id) ON DELETE CASCADE,
    size BIGINT,
    UNIQUE (filename, platform)
);
INSERT INTO files_new (filename, id, platform, package_id, size)
    SELECT filename, id, platform, package_id, size FROM files;
DROP TABLE files;
ALTER TABLE files_new RENAME TO files;
CREATE INDEX idx_files_package_id ON files (package_id);
";

/// The name is Postgres's own for the column constraint upstream declared, on
/// every database since: dropped by it, so a database that has something else
/// fails here rather than keeping the old constraint beside the new one.
const POSTGRES_UP: &str = "
ALTER TABLE public.files DROP CONSTRAINT files_filename_key;
ALTER TABLE public.files ADD CONSTRAINT files_filename_platform_key UNIQUE (filename, platform);
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let sql = match manager.get_database_backend() {
            DbBackend::Sqlite => SQLITE_UP,
            DbBackend::Postgres => POSTGRES_UP,
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        manager.get_connection().execute_unprepared(sql).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn a_file_name_is_unique_per_platform_only() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared("INSERT INTO packages (id, name) VALUES (1, 'pfetch')")
            .await
            .unwrap();
        let insert = |platform: &str| {
            format!(
                "INSERT INTO files (filename, platform, package_id) \
                 VALUES ('pfetch-1-1-any.pkg.tar.zst', '{platform}', 1)"
            )
        };

        db.execute_unprepared(&insert("x86_64")).await.unwrap();
        db.execute_unprepared(&insert("aarch64")).await.unwrap();
        assert!(
            db.execute_unprepared(&insert("x86_64")).await.is_err(),
            "the same platform twice"
        );
    }
}
