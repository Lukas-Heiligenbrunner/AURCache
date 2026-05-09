mod client;
mod deps;
mod model;
mod repo;

pub use client::{AurClient, snapshot_url};
pub use deps::{deps_from_srcinfo, parse_dep};
pub use model::{DependencyResolution, Error, Package, PkgDeps};
