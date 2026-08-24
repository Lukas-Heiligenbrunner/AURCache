//! Shared dependency-resolution helpers used by both the schema migration and
//! the `aurcache-utils` package pipeline.
//!
//! These live in `aurcache-db` because it is the lowest crate that can see both
//! the `packages` entity and the `AurClient` (via `aurcache-deps`), which lets
//! the migration and the runtime code share a single implementation.

use aurcache_deps::{AurClient, DependencyResolution, parse_dep};
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};
use std::collections::HashMap;

use crate::packages;

const ACTIVE_BUILD_STATUS: i32 = 0;
const SUCCESSFUL_BUILD_STATUS: i32 = 1;
const ENQUEUED_BUILD_STATUS: i32 = 3;

/// A package that can satisfy a dependency: either a row already in the
/// database, or one an in-flight add has planned but not yet inserted.
///
/// Only these three fields are ever consulted when matching, so planned
/// packages are represented directly rather than as `packages::Model`s with a
/// placeholder id — a fake id would be a trap for the next reader.
#[derive(Debug, Clone)]
pub struct PackageCandidate {
    /// The pkgbase, which is what a dependency ultimately resolves to.
    pub name: String,
    /// JSON array of split package names, as stored on `packages`.
    pub split_packages: Option<String>,
    /// JSON array of `provides` entries, as stored on `packages`.
    pub provides: Option<String>,
}

impl From<&packages::Model> for PackageCandidate {
    fn from(pkg: &packages::Model) -> Self {
        Self {
            name: pkg.name.clone(),
            split_packages: pkg.split_packages.clone(),
            provides: pkg.provides.clone(),
        }
    }
}

/// Resolve dependency names to their source (official / local repo / AUR).
///
/// Local matches (already-tracked packages, their split packages or provides)
/// take precedence; anything left over is resolved against the AUR/official
/// repositories via the [`AurClient`].
pub async fn resolve_dependency_resolutions<C: ConnectionTrait>(
    client: &AurClient,
    db: &C,
    dep_names: &[String],
) -> Result<HashMap<String, DependencyResolution>, aurcache_deps::Error> {
    resolve_dependency_resolutions_with_planned(client, db, dep_names, &[]).await
}

/// As [`resolve_dependency_resolutions`], but also considering packages an
/// in-flight add intends to insert.
///
/// An add resolves its whole dependency graph before writing anything, so a
/// package planned earlier in the same add is not yet visible in the database
/// — without this, a dependency satisfied by a sibling in the same add would
/// be resolved again against the AUR and planned twice.
pub async fn resolve_dependency_resolutions_with_planned<C: ConnectionTrait>(
    client: &AurClient,
    db: &C,
    dep_names: &[String],
    planned: &[PackageCandidate],
) -> Result<HashMap<String, DependencyResolution>, aurcache_deps::Error> {
    let mut resolutions = resolve_local_dependency_resolutions(db, dep_names, planned)
        .await
        .map_err(|e| aurcache_deps::Error::Rpc(e.to_string()))?;
    let unresolved = dep_names
        .iter()
        .filter(|dep_name| !resolutions.contains_key(dep_name.as_str()))
        .map(|dep_name| dep_name.as_str())
        .collect::<Vec<_>>();
    if unresolved.is_empty() {
        return Ok(resolutions);
    }

    resolutions.extend(client.resolve_dependencies(&unresolved).await?);
    Ok(resolutions)
}

async fn resolve_local_dependency_resolutions<C: ConnectionTrait>(
    db: &C,
    dep_names: &[String],
    planned: &[PackageCandidate],
) -> Result<HashMap<String, DependencyResolution>, DbErr> {
    let mut local_packages: Vec<PackageCandidate> = packages::Entity::find()
        .filter(packages::Column::Status.is_in([
            ACTIVE_BUILD_STATUS,
            SUCCESSFUL_BUILD_STATUS,
            ENQUEUED_BUILD_STATUS,
        ]))
        .all(db)
        .await?
        .iter()
        .map(PackageCandidate::from)
        .collect();
    local_packages.extend_from_slice(planned);

    Ok(dep_names
        .iter()
        .filter_map(|dep_name| {
            find_local_dependee_pkgbase(&local_packages, dep_name)
                .map(|pkgbase| (dep_name.clone(), DependencyResolution::Local { pkgbase }))
        })
        .collect())
}

fn find_local_dependee_pkgbase(
    local_packages: &[PackageCandidate],
    dep_name: &str,
) -> Option<String> {
    local_packages
        .iter()
        .filter_map(|pkg| local_match_rank(pkg, dep_name).map(|rank| (rank, pkg.name.as_str())))
        .min_by(|(left_rank, left_name), (right_rank, right_name)| {
            left_rank.cmp(right_rank).then(left_name.cmp(right_name))
        })
        .map(|(_, pkgbase)| pkgbase.to_string())
}

fn local_match_rank(pkg: &PackageCandidate, dep_name: &str) -> Option<u8> {
    if pkg.name == dep_name {
        return Some(0);
    }
    if json_list_contains(pkg.split_packages.as_deref(), dep_name, false) {
        return Some(1);
    }
    json_list_contains(pkg.provides.as_deref(), dep_name, true).then_some(2)
}

fn json_list_contains(json: Option<&str>, dep_name: &str, parse_relation: bool) -> bool {
    parse_json_list(json).into_iter().any(|value| {
        if parse_relation {
            parse_dep(&value).0 == dep_name
        } else {
            value == dep_name
        }
    })
}

fn parse_json_list(json: Option<&str>) -> Vec<String> {
    json.and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or_default()
}
