//! One module per route.

pub mod backup;
mod build;
mod builds;
pub mod config_files;
mod dashboard;
mod logs;
mod not_found;
pub mod package;
mod package_add;
mod package_builds;
mod package_config_files;
mod package_source;
mod packages;
pub mod settings;
pub mod worker;
pub mod workers;

pub use build::Build;
pub use builds::Builds;
pub use config_files::ConfigFiles;
pub use dashboard::Dashboard;
pub use logs::Logs;
pub use not_found::NotFound;
pub use package::{Package, PackageHeader};
pub use package_add::PackageAdd;
pub use package_builds::PackageBuilds;
pub use package_config_files::PackageConfigFiles;
pub use package_source::PackageSource;
pub use packages::Packages;
pub use settings::Settings;
pub use worker::{Worker, WorkerByFingerprint};
pub use workers::Workers;
