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

        match manager.get_database_backend() {
            DbBackend::Sqlite => {
                db.execute_unprepared(
                    r"
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
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
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
",
                )
                .await?;
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        }

        // Move package files into the new per-arch directory. A rename, not
        // copy-and-delete: same filesystem by construction, so it is atomic
        // and instant rather than doubling disk use on multi-gigabyte
        // packages — and failures are logged, not swallowed, because a file
        // left behind is a package the repository no longer serves.
        let src_path = Path::new("./repo");
        let dest_path = Path::new("./repo/x86_64");
        if let Err(e) = fs::create_dir_all(dest_path) {
            tracing::warn!("could not create {dest_path:?}, leaving repo files in place: {e}");
            return Ok(());
        }

        if let Ok(entries) = fs::read_dir(src_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let dest_file = dest_path.join(entry.file_name());
                // A previous partial run may have left the destination behind;
                // the old copy overwrote, so the rename does too.
                if dest_file.exists()
                    && let Err(e) = fs::remove_file(&dest_file)
                {
                    tracing::warn!("could not replace {dest_file:?}: {e}");
                    continue;
                }
                if let Err(e) = fs::rename(&path, &dest_file) {
                    tracing::warn!("could not move {path:?} to {dest_file:?}: {e}");
                }
            }
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match manager.get_database_backend() {
            DbBackend::Sqlite => {
                db.execute_unprepared(
                    r"
ALTER TABLE packages
DROP COLUMN build_flags;

ALTER TABLE packages
DROP COLUMN platforms;

ALTER TABLE builds
DROP COLUMN platform;

ALTER TABLE files
DROP COLUMN platform;
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
ALTER TABLE public.packages
DROP COLUMN build_flags;

ALTER TABLE public.packages
DROP COLUMN platforms;

ALTER TABLE public.builds
DROP COLUMN platform;

ALTER TABLE public.files
DROP COLUMN platform;
",
                )
                .await?;
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        }

        Ok(())
    }
}
