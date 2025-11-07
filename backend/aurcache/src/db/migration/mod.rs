pub use sea_orm_migration::prelude::*;

mod create;
mod m20240907_131839_platform_buildflags;
mod m20250213_223900_activity_log;
mod m20251015_230000_pkg_sources;
mod m20251106_100000_build_version;

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
        ]
    }
}
