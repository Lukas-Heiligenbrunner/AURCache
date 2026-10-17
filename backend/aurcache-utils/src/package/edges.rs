//! The dependency edges a package should have, from what its source declares.
//!
//! One reduction for the add path, which plans the rows, and the resync path,
//! which reconciles the ones already there: resolve every declared
//! dependency, drop the ones installable as binaries and the ones the package
//! answers to itself, and merge the bounds of every name that landed on the
//! same package base onto one edge.

use crate::pkg::{Constraint, Declared, DependencySet};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_db::helpers::dependency_resolution::{
    PackageCandidate, TrackedPackages, resolve_dependencies,
};
use aurcache_deps::{AurClient, DependencyResolution};
use std::collections::{HashMap, HashSet};

/// A package and what it declares.
pub(crate) struct Declaration<'a> {
    pub pkgbase: &'a str,
    pub deps: &'a DependencySet,
    /// Its split packages' names.
    pub pkgnames: &'a [String],
    pub provides: &'a [String],
}

/// Where a dependee comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dependee {
    /// Tracked here already, or planned earlier in the same add.
    Tracked,
    /// To be added from the AUR.
    Aur,
}

/// One edge: the package base depended on, and what is required of it.
pub(crate) struct Edge {
    pub pkgbase: String,
    /// Several when a range was declared, or several names landed here;
    /// none for an unversioned dependency.
    pub bounds: Vec<Constraint>,
    pub dependee: Dependee,
}

/// Resolve what `declaration` declares to its edges, in declared order.
///
/// `planned` are packages an in-flight add intends to insert, and `preferred`
/// the bases a package already depends on; see
/// [`resolve_dependencies`]. A dependency nothing provides is not fatal --
/// `makepkg` may still find it -- but it is said out loud, to `activity` or,
/// without one, to the journal: it is where a typo or a package dropped from
/// the AUR used to disappear without trace.
pub(crate) async fn resolve_edges(
    client: &AurClient,
    tracked: &TrackedPackages,
    declaration: &Declaration<'_>,
    planned: &[PackageCandidate],
    preferred: &HashSet<String>,
    activity: Option<&ActivityLog>,
) -> anyhow::Result<Vec<Edge>> {
    // What the package answers to itself is never an edge: resolving it would
    // let a co-provider (`flutter-bin` for `flutter`'s own `dart`) win.
    let self_provided = crate::pkg::self_provided_names(
        declaration.pkgbase,
        declaration.pkgnames,
        declaration.provides,
    );
    let declared: Vec<Declared> = declaration
        .deps
        .declared()
        .into_iter()
        .filter(|dep| !self_provided.contains(&dep.name))
        .collect();
    if declared.is_empty() {
        return Ok(Vec::new());
    }
    let resolved = resolve_dependencies(
        client,
        tracked,
        &declared
            .iter()
            .map(Declared::as_dependency)
            .collect::<Vec<_>>(),
        planned,
        preferred,
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!(
            "Failed to resolve dependencies for {}: {e}",
            declaration.pkgbase
        )
    })?;

    if !resolved.unresolved.is_empty() {
        match activity {
            Some(activity) => {
                for dependency in &resolved.unresolved {
                    activity.emit(Event::DepsUnresolved {
                        pkg: declaration.pkgbase.into(),
                        dependency: dependency.clone(),
                    });
                }
            }
            None => tracing::warn!(
                "{}: nothing provides {}",
                declaration.pkgbase,
                resolved.unresolved.join(", ")
            ),
        }
    }

    // In declared order rather than the resolution map's: a HashMap's order
    // varies per process, which would make the plan -- and the order builds
    // are queued in -- differ between runs for identical input.
    let mut edges: Vec<Edge> = Vec::new();
    let mut bounds: HashMap<String, Vec<Constraint>> = HashMap::new();
    for Declared { name, .. } in &declared {
        let (pkgbase, dependee) = match resolved.get(name) {
            // Installable as a binary: nothing to build, no row to link to.
            None | Some(DependencyResolution::Available) => continue,
            Some(DependencyResolution::Local { pkgbase }) => (pkgbase, Dependee::Tracked),
            Some(DependencyResolution::Aur { pkgbase }) => (pkgbase, Dependee::Aur),
        };
        if pkgbase == declaration.pkgbase {
            continue;
        }
        if !bounds.contains_key(pkgbase) {
            edges.push(Edge {
                pkgbase: pkgbase.clone(),
                bounds: Vec::new(),
                dependee,
            });
        }
        crate::pkg::merge_bounds_into(
            &mut bounds,
            pkgbase,
            declaration.deps.constraints.get(name),
        )?;
    }
    for edge in &mut edges {
        edge.bounds = bounds.remove(&edge.pkgbase).unwrap_or_default();
    }
    Ok(edges)
}
