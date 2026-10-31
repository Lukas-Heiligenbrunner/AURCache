//! Pointing one dependency edge somewhere else, or dropping it, by hand.
//!
//! Resolution picks what satisfies a dependency; this is how an operator
//! overrides it for one edge -- and the edge then survives later resolutions,
//! which prefer what a package already depends on.

use crate::package::live_check::live_check;
use crate::pkg::satisfies_constraint;
use crate::services::Services;
use aurcache_activitylog::events::Event;
use aurcache_common::api::package::{
    CandidateSource, DependencyCandidate, DependencyOptions, ReplacementVerdict,
};
use aurcache_db::helpers::builds::latest_successful_version_any_platform;
use aurcache_db::lists::json_list;
use aurcache_db::prelude::{Dependencies, Packages};
use aurcache_db::{dependencies, packages};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, FromQueryResult, ModelTrait,
    QueryFilter, QuerySelect, Set,
};
use std::fmt;

/// Why replacement options could not be offered, or a replacement not made.
#[derive(Debug)]
pub enum ReplaceError {
    /// No such package, or no such edge.
    NotFound(String),
    /// The request cannot be honoured as it stands; the message says why.
    Refused(String),
    /// Something it had to ask -- the AUR, the official repositories, a
    /// package's source -- could not be asked.
    Unreachable(anyhow::Error),
    /// The server's own failure.
    Internal(anyhow::Error),
}

impl fmt::Display for ReplaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(message) | Self::Refused(message) => f.write_str(message),
            Self::Unreachable(e) | Self::Internal(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ReplaceError {}

/// The package named `pkgbase`.
async fn package(db: &DatabaseConnection, pkgbase: &str) -> Result<packages::Model, ReplaceError> {
    Packages::find()
        .filter(packages::Column::Name.eq(pkgbase))
        .one(db)
        .await
        .map_err(|e| ReplaceError::Internal(e.into()))?
        .ok_or_else(|| ReplaceError::NotFound(format!("no package '{pkgbase}'")))
}

/// Every name a package answers to: its own, its split packages, and its
/// `provides` with any `=version` dropped.
///
/// This is what a replacement is measured against. A dependent's edge records
/// the constraint but not which of these names it declared, so covering all of
/// them is the only way to know a replacement covers a given dependent.
///
/// Fields, not the row: the options dialog matches over a narrowed fetch
/// ([`NamesRow`]) that never loads the rest of the columns -- source blobs,
/// patches.
fn provided_names(name: &str, split_packages: Option<&str>, provides: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> =
        crate::pkg::self_provided_names(name, &json_list(split_packages), &json_list(provides))
            .into_iter()
            .collect();
    names.sort_unstable();
    names
}

/// The columns [`provided_names`] reads, for matching over every package
/// without loading whole rows.
#[derive(FromQueryResult)]
struct NamesRow {
    id: i32,
    name: String,
    split_packages: Option<String>,
    provides: Option<String>,
}

impl NamesRow {
    fn names(&self) -> Vec<String> {
        provided_names(
            &self.name,
            self.split_packages.as_deref(),
            self.provides.as_deref(),
        )
    }
}

async fn official_holds(services: &Services, name: &str) -> Result<bool, ReplaceError> {
    services
        .client
        .official
        .holds(name)
        .await
        .map_err(|e| ReplaceError::Unreachable(e.into()))
}

/// One dependency edge and the packages on its ends.
struct DependencyEdge {
    dependent: packages::Model,
    /// The package currently satisfying the dependency.
    current: packages::Model,
    edge: dependencies::Model,
}

/// Resolve one dependency edge by the two package names on its ends.
async fn dependency_edge(
    db: &DatabaseConnection,
    dependent: &str,
    dependency: &str,
) -> Result<DependencyEdge, ReplaceError> {
    // The two endpoints are independent point lookups; the edge query below is
    // what genuinely depends on both.
    let (dependent, current) = tokio::join!(package(db, dependent), package(db, dependency),);
    let (dependent, current) = (dependent?, current?);
    let edge = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent.id))
        .filter(dependencies::Column::DependeeId.eq(current.id))
        .one(db)
        .await
        .map_err(|e| ReplaceError::Internal(e.into()))?
        .ok_or_else(|| {
            ReplaceError::NotFound(format!(
                "{} does not depend on {}",
                dependent.name, current.name
            ))
        })?;
    Ok(DependencyEdge {
        dependent,
        current,
        edge,
    })
}

/// The names `dependent` declares that `current` answers to.
///
/// The edge records the constraint but not the name behind it, so this reads
/// the dependent's source to recover it -- one source, already cached, for the
/// package whose page is asking. Empty means nothing the dependent declares
/// matches any more, which is a stale edge: droppable, but with nothing to
/// search for a replacement by.
async fn declared_names_for_edge(
    services: &Services,
    dependent: &packages::Model,
    current: &packages::Model,
) -> Result<Vec<String>, ReplaceError> {
    let sourceinfo = services
        .store
        .sourceinfo(&dependent.source_data, dependent.patch.as_deref())
        .await
        .map_err(ReplaceError::Unreachable)?;
    let deps = aurcache_deps::deps_from_srcinfo(
        &sourceinfo,
        &crate::pkg::architectures(&dependent.platforms),
    );
    let declared = crate::pkg::DependencySet::of(&deps).map_err(ReplaceError::Internal)?;

    let answers = provided_names(
        &current.name,
        current.split_packages.as_deref(),
        current.provides.as_deref(),
    );
    Ok(declared
        .names
        .into_iter()
        .filter(|name| answers.contains(name))
        .collect())
}

fn candidate_verdict(version: Option<&str>, constraint: &str) -> ReplacementVerdict {
    if constraint.is_empty() {
        return ReplacementVerdict::Satisfied;
    }
    match version {
        Some(version) if satisfies_constraint(version, constraint) => ReplacementVerdict::Satisfied,
        Some(_) => ReplacementVerdict::Unsatisfied,
        None => ReplacementVerdict::Unknown,
    }
}

pub async fn options(
    services: &Services,
    pkgbase: &str,
    dependency: &str,
) -> Result<DependencyOptions, ReplaceError> {
    let DependencyEdge {
        dependent,
        current,
        edge,
    } = dependency_edge(&services.db, pkgbase, dependency).await?;
    let declared_names = declared_names_for_edge(services, &dependent, &current).await?;

    let mut official = Vec::new();
    for name in &declared_names {
        if official_holds(services, name).await? {
            official.push(name.clone());
        }
    }

    // Tracked first: they are already here, so choosing one builds nothing new.
    // A package carrying a declared name outright leads one that merely
    // provides it, on the same rule resolution ranks by.
    //
    // Four narrowed columns, not whole rows: matching needs the id, the
    // name, and the two JSON name lists, and the rest (source blobs,
    // patches) would only ride along.
    let mut candidates = Vec::new();
    let tracked: Vec<NamesRow> = Packages::find()
        .select_only()
        .column(packages::Column::Id)
        .column(packages::Column::Name)
        .column(packages::Column::SplitPackages)
        .column(packages::Column::Provides)
        .into_model()
        .all(&services.db)
        .await
        .map_err(|e| ReplaceError::Internal(e.into()))?;
    let mut tracked_matches: Vec<&NamesRow> = tracked
        .iter()
        .filter(|package| package.id != current.id && package.id != dependent.id)
        .filter(|package| {
            package
                .names()
                .iter()
                .any(|name| declared_names.contains(name))
        })
        .collect();
    tracked_matches.sort_by_key(|package| {
        (
            !declared_names.contains(&package.name),
            package.name.as_str(),
        )
    });
    for package in tracked_matches {
        // One indexed point lookup per match, not a batched scan: matches
        // are a handful of rows, and "latest" means newest end time, which
        // no GROUP BY over ids reproduces.
        let version = latest_successful_version_any_platform(&services.db, package.id)
            .await
            .map_err(|e| ReplaceError::Internal(e.into()))?;
        candidates.push(DependencyCandidate {
            pkgbase: package.name.clone(),
            source: CandidateSource::Tracked,
            verdict: candidate_verdict(version.as_deref(), &edge.version_constraint),
            version,
        });
    }

    // Then the AUR, in the order resolution itself would rank them, minus
    // everything already offered above.
    let tracked_names: std::collections::HashSet<&str> = tracked
        .iter()
        .map(|package| package.name.as_str())
        .collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut aur_error = None;
    for name in &declared_names {
        let providers = match services.client.aur_providers(name).await {
            Ok(providers) => providers,
            // Best-effort: an unreachable AUR must not take the tracked
            // candidates down with it, and the caller is told which half of
            // the answer is missing rather than left to read an empty list as
            // "nothing exists".
            Err(e) => {
                aur_error = Some(e.to_string());
                break;
            }
        };
        for package in providers {
            if tracked_names.contains(package.package_base.as_str())
                || !seen.insert(package.package_base.clone())
            {
                continue;
            }
            candidates.push(DependencyCandidate {
                pkgbase: package.package_base,
                source: CandidateSource::Aur,
                verdict: candidate_verdict(Some(&package.version), &edge.version_constraint),
                version: Some(package.version),
            });
        }
    }

    Ok(DependencyOptions {
        dependent: dependent.name,
        current: current.name,
        version_constraint: edge.version_constraint,
        declared_names,
        official,
        candidates,
        aur_error,
    })
}

pub async fn replace(
    services: &Services,
    pkgbase: &str,
    dependency: &str,
    replacement: Option<String>,
    user: Option<String>,
) -> Result<(), ReplaceError> {
    let DependencyEdge {
        dependent,
        current,
        edge,
    } = dependency_edge(&services.db, pkgbase, dependency).await?;
    let declared_names = declared_names_for_edge(services, &dependent, &current).await?;

    match replacement {
        None => {
            // Dropping is only honest when nothing has to be built for the
            // name any more. Otherwise the edge would come straight back the
            // next time the dependent is resolved, and the button would look
            // like it had failed.
            for name in &declared_names {
                if !official_holds(services, name).await? {
                    return Err(ReplaceError::Refused(format!(
                        "'{name}' is not published by the official repositories, so this dependency cannot be dropped"
                    )));
                }
            }
            edge.delete(&services.db)
                .await
                .map_err(|e| ReplaceError::Internal(e.into()))?;
            services.activity.emit_by(
                Event::DepsDropped {
                    dependent: dependent.name.as_str().into(),
                    dependency: current.name.as_str().into(),
                },
                user.clone(),
            );
        }
        Some(replacement) => {
            if replacement == current.name {
                return Err(ReplaceError::Refused(format!(
                    "{} already depends on {replacement}",
                    dependent.name
                )));
            }
            if replacement == dependent.name {
                return Err(ReplaceError::Refused(
                    "a package cannot depend on itself".to_string(),
                ));
            }
            if declared_names.is_empty() {
                return Err(ReplaceError::Refused(format!(
                    "{} no longer declares anything {} answers to, so there is nothing to replace -- drop the dependency instead",
                    dependent.name, current.name
                )));
            }

            let package = ensure_replacement_exists(services, &dependent, &replacement).await?;
            let answers = provided_names(
                &package.name,
                package.split_packages.as_deref(),
                package.provides.as_deref(),
            );
            if !declared_names.iter().any(|name| answers.contains(name)) {
                return Err(ReplaceError::Refused(format!(
                    "{replacement} answers to none of {}, so the edge would be undone at the next update",
                    declared_names.join(", ")
                )));
            }

            repoint_edge(&services.db, edge, package.id)
                .await
                .map_err(|e| ReplaceError::Internal(e.into()))?;
            services.activity.emit_by(
                Event::DepsReplaced {
                    dependent: dependent.name.as_str().into(),
                    old: current.name.as_str().into(),
                    new: package.name.as_str().into(),
                },
                user.clone(),
            );
        }
    }

    // The dependent's queue entry was made against the edge that just changed,
    // so it may now be wrong in either direction: free to start because what
    // held it up is no longer its dependency, or obliged to wait because the
    // replacement has not been built yet. Before `live_check`, which may delete
    // the old dependency and everything that hung off it.
    let unblocked = crate::worker_complete::resync_pending_builds(&services.db, dependent.id)
        .await
        .map_err(|e| ReplaceError::Internal(e.into()))?;
    for build in aurcache_db::helpers::builds::build_refs(&services.db, &unblocked)
        .await
        .map_err(|e| ReplaceError::Internal(e.into()))?
    {
        services
            .activity
            .emit_by(Event::BuildUnblocked { build, by: None }, user.clone());
    }

    // The usual collection, now that the old dependency may be holding nothing
    // up. This is what makes emptying a package's dependents remove it: patch
    // the last edge away and the package goes with it, without a second
    // endpoint that knows how to remove packages.
    live_check(
        &services.db,
        &services.store,
        &services.repo,
        &[current.id],
        &[],
    )
    .await
    .map_err(ReplaceError::Internal)?;

    Ok(())
}

/// Move `edge` onto `replacement_id`.
///
/// There is one row per (dependent, dependency) pair, and the dependent may
/// already depend on the replacement under some other name, so an edge that
/// would collide is merged into the one already there rather than duplicated.
/// Neither constraint is merged into the other: both are recomputed from the
/// dependent's declarations at its next resync, and guessing here would only
/// disagree with that in the meantime.
async fn repoint_edge(
    db: &DatabaseConnection,
    edge: dependencies::Model,
    replacement_id: i32,
) -> Result<(), sea_orm::DbErr> {
    let collides = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(edge.dependent_id))
        .filter(dependencies::Column::DependeeId.eq(replacement_id))
        .one(db)
        .await?
        .is_some();

    if collides {
        edge.delete(db).await?;
    } else {
        let mut active: dependencies::ActiveModel = edge.into();
        active.dependee_id = Set(replacement_id);
        active.save(db).await?;
    }
    Ok(())
}

/// The row to point an edge at, adding it from the AUR if it is not here yet.
///
/// Added the way resolution would have added it -- as a dependency, on the
/// dependent's own platforms and build flags -- so a replacement chosen by
/// hand is indistinguishable from one resolution picked itself. That includes
/// its builds: it is queued like any added package, and the dependent's own
/// build then waits on it rather than on a package nothing will ever build.
async fn ensure_replacement_exists(
    services: &Services,
    dependent: &packages::Model,
    replacement: &str,
) -> Result<packages::Model, ReplaceError> {
    if let Some(package) = Packages::find()
        .filter(packages::Column::Name.eq(replacement))
        .one(&services.db)
        .await
        .map_err(|e| ReplaceError::Internal(e.into()))?
    {
        return Ok(package);
    }

    crate::package::add::add_dependency(services, dependent, replacement)
        .await
        .map_err(ReplaceError::Unreachable)?;

    Packages::find()
        .filter(packages::Column::Name.eq(replacement))
        .one(&services.db)
        .await
        .map_err(|e| ReplaceError::Internal(e.into()))?
        .ok_or_else(|| {
            ReplaceError::NotFound(format!("'{replacement}' could not be added from the AUR"))
        })
}

#[cfg(test)]
mod dependency_tests {
    use super::{
        ReplacementVerdict, candidate_verdict, dependency_edge, provided_names, repoint_edge,
    };
    use crate::package::live_check::live_check;
    use crate::repository::Repository;
    use crate::snapshot::SnapshotStore;
    use aurcache_common::build_state::BuildState;
    use aurcache_db::migration::Migrator;
    use aurcache_db::packages::SourceData;
    use aurcache_db::prelude::{Dependencies, Packages};
    use aurcache_db::{dependencies, packages};
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, Database, DatabaseConnection, EntityTrait, PaginatorTrait,
        QueryFilter, Set, TryIntoModel,
    };
    use sea_orm_migration::MigratorTrait;
    use serde_json::json;

    async fn memory_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    async fn package(
        db: &DatabaseConnection,
        name: &str,
        directly_requested: bool,
    ) -> packages::Model {
        packages::ActiveModel {
            name: Set(name.to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(None),
            build_flags: Set(Default::default()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur { name: name.into() }),
            directly_requested: Set(directly_requested),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap()
    }

    async fn edge(db: &DatabaseConnection, dependent: i32, dependee: i32, constraint: &str) {
        dependencies::ActiveModel {
            dependent_id: Set(dependent),
            dependee_id: Set(dependee),
            version_constraint: Set(constraint.to_string()),
            ..Default::default()
        }
        .save(db)
        .await
        .unwrap();
    }

    /// A replacement is judged against every name the package answers to, so
    /// all three sources of them have to be here -- and a versioned `provides`
    /// contributes the name, not the whole entry.
    #[tokio::test]
    async fn provided_names_covers_the_name_the_splits_and_the_provides() {
        let db = memory_db().await;
        let package = package(&db, "libfoo", true).await;
        assert_eq!(
            provided_names(
                &package.name,
                package.split_packages.as_deref(),
                package.provides.as_deref()
            ),
            vec!["libfoo"]
        );

        let split_packages = Some(json!(["libfoo-docs"]).to_string());
        let provides = Some(json!(["libfoo.so=1", "foo-compat"]).to_string());
        assert_eq!(
            provided_names(
                &package.name,
                split_packages.as_deref(),
                provides.as_deref()
            ),
            vec!["foo-compat", "libfoo", "libfoo-docs", "libfoo.so"],
            "a versioned `provides` contributes the name, not the whole entry"
        );
    }

    /// An unbuilt candidate is `Unknown`, not `Unsatisfied`: the queue checks
    /// the constraint against each real build, so there is nothing to conclude
    /// yet and saying "no" would hide a usable option.
    #[test]
    fn a_candidate_is_judged_on_the_version_it_is_known_to_be_at() {
        assert_eq!(
            candidate_verdict(None, ""),
            ReplacementVerdict::Satisfied,
            "an unconstrained edge is met by anything"
        );
        assert_eq!(
            candidate_verdict(Some("2.0.0-1"), ">=2.0"),
            ReplacementVerdict::Satisfied
        );
        assert_eq!(
            candidate_verdict(Some("1.0.0-1"), ">=2.0"),
            ReplacementVerdict::Unsatisfied
        );
        assert_eq!(
            candidate_verdict(None, ">=2.0"),
            ReplacementVerdict::Unknown
        );
    }

    #[tokio::test]
    async fn an_edge_that_does_not_exist_is_not_found() {
        let db = memory_db().await;
        package(&db, "dependent", true).await;
        package(&db, "stranger", false).await;

        assert!(dependency_edge(&db, "dependent", "stranger").await.is_err());
        assert!(dependency_edge(&db, "dependent", "nonesuch").await.is_err());
    }

    /// Repointing the last edge onto something else leaves the old dependency
    /// holding nothing up, and the collection that follows takes it. This is
    /// what lets emptying a package's dependents remove it, with no second
    /// endpoint that knows how to remove packages.
    #[tokio::test]
    async fn repointing_the_last_edge_collects_the_old_dependency() {
        let db = memory_db().await;
        let dependent = package(&db, "dependent", true).await;
        let old = package(&db, "old-provider", false).await;
        let new = package(&db, "new-provider", false).await;
        edge(&db, dependent.id, old.id, ">=1.0").await;

        let moving = Dependencies::find().one(&db).await.unwrap().unwrap();
        repoint_edge(&db, moving, new.id).await.unwrap();
        let checkouts = tempfile::tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root(checkouts.path().to_path_buf());
        let repo = Repository::new(checkouts.path().join("repo"));
        live_check(&db, &store, &repo, &[old.id], &[])
            .await
            .unwrap();

        assert!(
            Packages::find_by_id(old.id)
                .one(&db)
                .await
                .unwrap()
                .is_none(),
            "nothing needs the old dependency any more"
        );
        let remaining = Dependencies::find().all(&db).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].dependee_id, new.id);
        assert_eq!(
            remaining[0].version_constraint, ">=1.0",
            "the constraint travels with the edge"
        );
    }

    /// A dependent that already depends on the replacement under some other
    /// name would collide, since there is one row per pair. The edge is merged
    /// into the one already there rather than duplicated.
    #[tokio::test]
    async fn repointing_onto_an_edge_that_already_exists_merges() {
        let db = memory_db().await;
        let dependent = package(&db, "dependent", true).await;
        let old = package(&db, "old-provider", false).await;
        let new = package(&db, "new-provider", false).await;
        edge(&db, dependent.id, old.id, ">=1.0").await;
        edge(&db, dependent.id, new.id, ">=3.0").await;

        let moving = Dependencies::find()
            .filter(dependencies::Column::DependeeId.eq(old.id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        repoint_edge(&db, moving, new.id).await.unwrap();

        assert_eq!(Dependencies::find().count(&db).await.unwrap(), 1);
        let remaining = Dependencies::find().one(&db).await.unwrap().unwrap();
        assert_eq!(remaining.dependee_id, new.id);
        assert_eq!(remaining.version_constraint, ">=3.0");
    }

    /// A dependency something else still needs survives the repoint: the
    /// collection is reachability, not "did an edge just move".
    #[tokio::test]
    async fn a_dependency_another_package_still_needs_is_kept() {
        let db = memory_db().await;
        let dependent = package(&db, "dependent", true).await;
        let other = package(&db, "other", true).await;
        let old = package(&db, "old-provider", false).await;
        let new = package(&db, "new-provider", false).await;
        edge(&db, dependent.id, old.id, "").await;
        edge(&db, other.id, old.id, "").await;

        let moving = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(dependent.id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        repoint_edge(&db, moving, new.id).await.unwrap();
        let checkouts = tempfile::tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root(checkouts.path().to_path_buf());
        let repo = Repository::new(checkouts.path().join("repo"));
        live_check(&db, &store, &repo, &[old.id], &[])
            .await
            .unwrap();

        assert!(
            Packages::find_by_id(old.id)
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
    }
}
