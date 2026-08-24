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
    let mut resolutions = resolve_local_dependency_resolutions(db, dep_names)
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
) -> Result<HashMap<String, DependencyResolution>, DbErr> {
    let local_packages = packages::Entity::find()
        .filter(packages::Column::Status.is_in(vec![
            ACTIVE_BUILD_STATUS,
            SUCCESSFUL_BUILD_STATUS,
            ENQUEUED_BUILD_STATUS,
        ]))
        .all(db)
        .await?;

    Ok(dep_names
        .iter()
        .filter_map(|dep_name| {
            find_local_dependee_pkgbase(&local_packages, dep_name)
                .map(|pkgbase| (dep_name.clone(), DependencyResolution::Local { pkgbase }))
        })
        .collect())
}

fn find_local_dependee_pkgbase(
    local_packages: &[packages::Model],
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

fn local_match_rank(pkg: &packages::Model, dep_name: &str) -> Option<u8> {
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
