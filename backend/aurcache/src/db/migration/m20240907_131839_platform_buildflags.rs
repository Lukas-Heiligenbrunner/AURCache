use crate::db::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;
use std::fs;
use std::path::Path;

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
ALTER TABLE packages
ADD COLUMN build_flags TEXT;

ALTER TABLE packages
ADD COLUMN platforms TEXT;

ALTER TABLE builds
ADD COLUMN platform TEXT;

ALTER TABLE files
ADD COLUMN platform TEXT;

UPDATE packages
SET build_flags = '-Syu;--noconfirm;--noprogressbar;--color never',
    platforms = 'x86_64';

UPDATE builds
    SET platform = 'x86_64';

UPDATE files
    SET platform = 'x86_64';
"#,
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r#"
ALTER TABLE public.packages
ADD COLUMN build_flags TEXT;

ALTER TABLE public.packages
ADD COLUMN platforms TEXT;

ALTER TABLE public.builds
ADD COLUMN platform TEXT;

ALTER TABLE public.files
ADD COLUMN platform TEXT;

UPDATE public.packages
SET build_flags = '-Syu;--noconfirm;--noprogressbar;--color never',
    platforms = 'x86_64';

UPDATE public.builds
    SET platform = 'x86_64';

UPDATE public.files
    SET platform = 'x86_64';
"#,
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        // try to copy pkg files to new location
        let src_path = Path::new("./repo");
        let dest_path = Path::new("./repo/x86_64");

        // Iterate over the files in the source directory
        if let Ok(entries) = fs::read_dir(src_path) {
            for entry in entries.flatten() {
                let path = entry.path();

                // Only copy files (not directories)
                if path.is_file() {
                    let file_name = entry.file_name();
                    let dest_file = dest_path.join(file_name);

                    // Copy the file to the destination directory
                    _ = fs::copy(path.clone(), dest_file);
                    _ = fs::remove_file(path);
                }
            }
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
DROP COLUMN build_flags;

ALTER TABLE packages
DROP COLUMN platforms;

ALTER TABLE builds
DROP COLUMN platform;

ALTER TABLE files
DROP COLUMN platform;
"#,
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r#"
ALTER TABLE public.packages
DROP COLUMN build_flags;

ALTER TABLE public.packages
DROP COLUMN platforms;

ALTER TABLE builds
DROP COLUMN platform;

ALTER TABLE files
DROP COLUMN platform;
"#,
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}
