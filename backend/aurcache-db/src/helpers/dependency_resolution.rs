//! Dependency resolution against the packages AURCache tracks.
//!
//! This lives in `aurcache-db` because it is the lowest crate that can see
//! both the `packages` entity and the `AurClient` (via `aurcache-deps`), which
//! lets the schema migration and the runtime package pipeline share one
//! implementation.
//!
//! It is deliberately thin. Deciding *what satisfies a dependency name* is
//! `aurcache-deps`' job and is the same question whatever the source, so all
//! this does is describe the tracked packages in the shape
//! [`SatisfyIndex`] understands and hand it over. Matching, ranking and
//! ordering all happen once, in
//! [`AurClient::resolve_dependencies`](aurcache_deps::AurClient::resolve_dependencies).

use aurcache_deps::{AurClient, Dependency, Resolutions, SatisfyIndex};
use sea_orm::{ConnectionTrait, DbErr, EntityTrait, FromQueryResult, QuerySelect};
use std::collections::HashSet;

use crate::lists::json_list;
use crate::packages;

/// A package that can satisfy a dependency: either a row already in the
/// database, or one an in-flight add has planned but not yet inserted.
///
/// Only these three fields are ever consulted when matching, so planned
/// packages are represented directly rather than as `packages::Model`s with a
/// placeholder id — a fake id would be a trap for the next reader.
///
/// Read straight off the row: the selected columns below are exactly these
/// fields, so there is no intermediate shape to keep in sync.
#[derive(Debug, Clone, FromQueryResult)]
pub struct PackageCandidate {
    /// The pkgbase, which is what a dependency ultimately resolves to.
    pub name: String,
    /// JSON array of split package names, as stored on `packages`.
    pub split_packages: Option<String>,
    /// JSON array of `provides` entries, as stored on `packages`.
    pub provides: Option<String>,
}

/// The packages AURCache tracks, read once and reused.
///
/// Resolution needs every row, because a dependency can be satisfied by a
/// package's own name, one of its split packages, or something it declares in
/// `provides` -- none of which a `WHERE` clause can match against, since the
/// last two are JSON columns. That makes each read a full table scan, so how
/// long one snapshot may be reused is a correctness question, and a deliberate
/// one:
///
/// - **An add** loads once. It resolves its whole graph before writing
///   anything -- `persist_plan` does every insert at the end, in one
///   transaction -- so the rows cannot change underneath it, and the packages
///   it intends to add travel separately as `planned`. Planning a package with
///   a hundred dependencies used to re-read the table once per package
///   planned.
/// - **A resync** loads once; it resolves a single package.
/// - **The backfill migration** must load per package: it inserts rows as it
///   recurses, so a snapshot would go stale and a package inserted earlier in
///   the run would be resolved against the AUR and added a second time.
///
/// Held as a named value rather than cached invisibly so that each caller's
/// choice is one it had to make.
pub struct TrackedPackages {
    candidates: Vec<PackageCandidate>,
}

impl TrackedPackages {
    /// Read every tracked package.
    ///
    /// Every row counts, whatever state its last build left it in. A row's
    /// status says how its build went, not whether a dependency on it is real,
    /// and what actually gates a dependent is the dependee's latest
    /// *successful build* ([`crate::helpers::builds::dependency_satisfied`]) —
    /// which no filter here could speak for. Restricting this to
    /// `ACTIVE`/`SUCCESS`/`ENQUEUED`, as it once did, only meant a `Failed` or
    /// `WaitingForDeps` package went unrecognised here and was picked up a
    /// stage later by its artifact sitting in the repository, which reports
    /// "already published" and records no dependency link at all.
    pub async fn load<C: ConnectionTrait>(db: &C) -> Result<Self, DbErr> {
        // Only the three columns matching consults. The rows also carry the
        // large `source_data` JSON, which a full-model load would haul in for
        // every package on every resolution.
        let candidates = packages::Entity::find()
            .select_only()
            .column(packages::Column::Name)
            .column(packages::Column::SplitPackages)
            .column(packages::Column::Provides)
            .into_model::<PackageCandidate>()
            .all(db)
            .await?;
        Ok(Self { candidates })
    }

    /// Index the loaded rows, plus anything `planned`, under the names each
    /// one answers to -- keeping only the names in `wanted`.
    fn index(&self, planned: &[PackageCandidate], wanted: &HashSet<&str>) -> SatisfyIndex {
        let mut index = SatisfyIndex::new();
        for candidate in self.candidates.iter().chain(planned) {
            // A row's own name is its pkgbase, and it carries the `provides`
            // for the base as a whole.
            index.insert_package(
                &candidate.name,
                &candidate.name,
                // Nothing here has a version: these are matched to decide
                // whether a dependency *edge* should exist, and the edge
                // records the constraint for the build queue to check against
                // real builds.
                None,
                json_list(candidate.provides.as_deref()),
                wanted,
            );
            for split in json_list(candidate.split_packages.as_deref()) {
                index.insert_package(&split, &candidate.name, None, Vec::<String>::new(), wanted);
            }
        }
        index
    }
}

/// Resolve `deps` to what should happen about each of them.
///
/// `planned` are packages an in-flight add intends to insert. An add resolves
/// its whole dependency graph before writing anything, so a package planned
/// earlier in the same add is not yet in the database — without offering them
/// here, a dependency satisfied by a sibling in the same add would be resolved
/// against the AUR and planned a second time. Callers with nothing in flight
/// pass `&[]`.
///
/// `preferred` are package bases to favour among the tracked candidates; see
/// [`AurClient::resolve_dependencies`]. A package being re-resolved passes the
/// bases it already depends on, so an edge someone repointed by hand is not
/// undone. An add has no previous edges and passes an empty set.
pub async fn resolve_dependencies(
    client: &AurClient,
    tracked: &TrackedPackages,
    deps: &[Dependency<'_>],
    planned: &[PackageCandidate],
    preferred: &HashSet<String>,
) -> Result<Resolutions, aurcache_deps::Error> {
    let wanted = deps.iter().map(|dep| dep.name).collect();
    client
        .resolve_dependencies(deps, &tracked.index(planned, &wanted), preferred)
        .await
}
