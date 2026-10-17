//! Everything AURCache's schema gained after upstream 0.5.0, in one step.
//!
//! It grew over some forty migrations that no public instance ever ran, so
//! they are folded into this one: what 0.5.0 lacks is created in its final
//! shape, and the conversions a 0.5.0 database needs run in the order their
//! data depends on.
//!
//! 1. Tables 0.5.0 does not have, and the columns its own tables gain --
//!    first, so every conversion reads and writes the final columns.
//! 2. The conversions: packages, then builds, then settings.
//! 3. What 0.5.0 stored and nothing reads any more, dropped; builds and files
//!    tied to their package.
//! 4. Indexes, once the data satisfies the unique ones.
//!
//! There is no way back to 0.5.0 from here, so `down` refuses: a dump
//! restored into a 0.5.0 instance is the route back.

mod builds;
mod packages;
mod schema;
mod settings;
#[cfg(test)]
mod tests;

use sea_orm_migration::prelude::*;

pub struct Migration;

/// Named by hand: `DeriveMigrationName` takes the source file's name, which
/// for a module in a directory is `mod`.
impl MigrationName for Migration {
    fn name(&self) -> &'static str {
        "m20261008_000000_from_0_5"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        schema::create_tables(manager).await?;
        schema::add_columns(manager).await?;

        packages::normalize_build_flags_in_db(db).await?;
        packages::merge_file_links(manager).await?;
        packages::normalize_package_names_and_merge_duplicates(db).await?;
        builds::fail_unknown_statuses(manager).await?;
        packages::mark_duplicate_pending_builds_failed(db).await?;
        builds::number_builds(manager).await?;
        builds::fill_start_times(manager).await?;
        builds::move_logs_to_files(manager).await?;
        builds::delete_orphans(manager).await?;
        builds::measure_outputs(manager).await?;
        builds::derive_package_statuses(manager).await?;
        settings::retire(manager).await?;
        settings::convert_schedule(manager).await?;
        packages::queue_dependency_backfill(manager).await?;
        packages::patch_repo_packager().map_err(|e| DbErr::Custom(e.to_string()))?;

        schema::drop_columns(manager).await?;
        schema::add_constraints(manager).await?;
        schema::create_indexes(manager).await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Migration(
            "a database cannot be taken back to AURCache 0.5.0; \
             restore a dump into a 0.5.0 instance instead"
                .to_string(),
        ))
    }
}
