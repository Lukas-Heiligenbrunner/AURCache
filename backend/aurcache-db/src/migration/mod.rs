use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

/// What a raw statement prefixes table names with on `backend`.
fn schema_prefix(backend: DbBackend) -> &'static str {
    if backend == DbBackend::Postgres {
        "public."
    } else {
        ""
    }
}

mod create;
mod m20240907_131839_platform_buildflags;
mod m20250213_223900_activity_log;
mod m20251015_230000_pkg_sources;
mod m20251106_100000_build_version;
mod m20251107_000000_build_flags_no_install;
mod m20251204_160000_settings;
pub mod m20261008_000000_from_0_5;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(create::Migration),
            Box::new(m20240907_131839_platform_buildflags::Migration),
            Box::new(m20250213_223900_activity_log::Migration),
            Box::new(m20251106_100000_build_version::Migration),
            Box::new(m20251015_230000_pkg_sources::Migration),
            Box::new(m20251204_160000_settings::Migration),
            Box::new(m20251107_000000_build_flags_no_install::Migration),
            Box::new(m20261008_000000_from_0_5::Migration),
        ]
    }
}
