mod client;
mod deps;
mod model;
pub mod paths;
mod repo;
mod satisfy;
mod version;

pub use client::AurClient;
pub use deps::{deps_from_srcinfo, parse_dep};
pub use model::{Dependency, DependencyResolution, Error, Package, PkgDeps, Resolutions};
pub use satisfy::{Match, MatchKind, SatisfyIndex};
pub use version::satisfies_constraint;

/// Convert a search result into the API shape.
///
/// Implemented here rather than in the server: `Package` is defined in this
/// crate and `ApiPackage` in `aurcache-common`, so the server — owning
/// neither — cannot write this impl.
impl From<Package> for aurcache_common::api::aur::ApiPackage {
    fn from(package: Package) -> Self {
        Self {
            name: package.name,
            version: package.version,
            description: package.description,
        }
    }
}
