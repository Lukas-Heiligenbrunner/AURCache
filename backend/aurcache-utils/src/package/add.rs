use crate::package::edges::{Declaration, Dependee, resolve_edges};
use crate::package::enqueue::trigger_initial_builds;
use crate::package::metadata::refresh_source_metadata;
use crate::patch::SourcePatch;
use crate::services::Services;
use crate::snapshot::SnapshotStore;
use anyhow::{anyhow, bail};
use async_recursion::async_recursion;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_common::build_state::BuildState;
use aurcache_db::helpers::dependency_resolution::{PackageCandidate, TrackedPackages};
use aurcache_db::lists::{BuildFlags, Platforms};
use aurcache_db::packages;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::Packages;
use pacman_mirrors::platforms::Platform;
use sea_orm::QueryFilter;
use sea_orm::prelude::Expr;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait,
    TransactionTrait,
};
use std::collections::{BTreeMap, HashMap, HashSet};

/// How the packages an add inserts are to be built.
pub(crate) struct AddContext {
    platforms: Platforms,
    build_flags: BuildFlags,
}

impl AddContext {
    /// The context `dependent` was added with, for dependencies added on its
    /// behalf: they are built the way the package needing them is.
    fn of(dependent: &packages::Model) -> Self {
        Self {
            platforms: dependent.platforms.clone(),
            build_flags: dependent.build_flags.clone(),
        }
    }

    /// The architectures whose dependencies the packages need.
    fn architectures(&self) -> Vec<alpm_types::SystemArchitecture> {
        crate::pkg::architectures(&self.platforms)
    }
}

struct PackageInsertSpec {
    pkgbase: String,
    version: String,
    deps: crate::pkg::DependencySet,
    pkgnames: Vec<String>,
    provides: Vec<String>,
    source_data: SourceData,
    patch: Option<String>,
}

/// A package an add intends to insert, with everything needed to write the row.
struct PlannedPackage {
    pkgbase: String,
    version: String,
    source_data: SourceData,
    patch: Option<String>,
    split_packages: Option<String>,
    provides: Option<String>,
    /// Only the package the user actually asked for; dependencies are not.
    directly_requested: bool,
}

/// What a plan is built against: services and settings, all read-only for the
/// duration of the plan.
///
/// Grouped for the same reason `update`'s `Services` is -- the planning
/// recursion threads every one of these through unchanged, and passing them
/// individually made each hop a seven-argument call.
struct PlanContext<'a> {
    client: &'a aurcache_deps::AurClient,
    store: &'a SnapshotStore,
    db: &'a DatabaseConnection,
    /// The tracked packages, read once for the whole plan.
    tracked: &'a TrackedPackages,
    context: &'a AddContext,
    /// Where a dependency nothing provides is recorded; `None` for a caller
    /// with no log to hand, which leaves it to the journal.
    activity: Option<&'a ActivityLog>,
}

/// A dependency edge, held by *name* because planned packages have no id until
/// the plan is persisted.
struct PlannedEdge {
    dependent: String,
    dependee: String,
    version_constraint: String,
}

/// Everything an add will write, resolved before anything is written.
///
/// Resolution walks the AUR and downloads repo databases, so doing it inside a
/// transaction would hold a write lock across the network — on SQLite that
/// blocks worker claims and build status updates for the duration. Planning
/// first keeps the transaction short *and* makes the add atomic: a failure
/// part-way through leaves nothing behind, where previously each package was
/// committed as it was resolved and a later failure left orphan rows that went
/// on to satisfy dependencies for subsequent adds.
#[derive(Default)]
struct AddPlan {
    /// Dependency-first, so inserting in order satisfies edges as they appear.
    packages: Vec<PlannedPackage>,
    edges: Vec<PlannedEdge>,
}

impl AddPlan {
    /// The planned packages as dependency-resolution candidates, so a package
    /// planned earlier in this add can satisfy a later one.
    fn candidates(&self) -> Vec<PackageCandidate> {
        self.packages
            .iter()
            .map(|pkg| PackageCandidate {
                name: pkg.pkgbase.clone(),
                split_packages: pkg.split_packages.clone(),
                provides: pkg.provides.clone(),
            })
            .collect()
    }
}

pub(crate) fn build_add_context(
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
) -> AddContext {
    // Platform names are validated where they enter as strings (the API's
    // `Platform::from_str`); by the time they are `Platform` values there is
    // nothing left to check.
    AddContext {
        platforms: Platforms::new(platforms.unwrap_or_else(|| vec![Platform::X86_64])),
        build_flags: build_flags.map_or_else(
            || BuildFlags::new(["--noconfirm", "--noprogressbar", "--nocolor"]),
            BuildFlags::new,
        ),
    }
}

pub(crate) async fn package_exists(db: &DatabaseConnection, pkgbase: &str) -> anyhow::Result<bool> {
    Ok(Packages::find()
        .filter(packages::Column::Name.eq(pkgbase))
        .one(db)
        .await?
        .is_some())
}

async fn resolve_aur_pkgbase(
    client: &aurcache_deps::AurClient,
    package_name: &str,
) -> anyhow::Result<String> {
    let pkg_name = package_name.trim();
    let bases = client
        .resolve_bases(&[pkg_name])
        .await
        .map_err(|e| anyhow!("AUR lookup failed: {e}"))?;

    // If the name resolves to a pkgbase via RPC (e.g. "czkawka-cli" → "czkawka")
    // use that; otherwise assume the input already *is* the pkgbase (e.g. "czkawka"
    // when it has no child package of the same name).
    Ok(bases
        .get(pkg_name)
        .cloned()
        .unwrap_or_else(|| pkg_name.to_string()))
}

/// Build a [`SourcePatch`] from a caller-supplied map of full file contents
/// (path -> new content), diffing each against the source's current
/// pristine content. Since the diff is always computed fresh against the
/// current pristine base, the resulting patch is guaranteed to apply -
/// no separate "does it apply" validation is needed.
async fn build_patch_from_files(
    store: &SnapshotStore,
    source_data: &SourceData,
    patched_files: BTreeMap<String, String>,
) -> anyhow::Result<Option<String>> {
    let mut patch = SourcePatch::default();
    for (path, new_content) in patched_files {
        let original = store.read_file(source_data, None, &path).await?;
        patch.merge_file(&path, &original, &new_content);
    }
    patch.stored()
}

async fn resolve_srcinfo_to_spec(
    store: &SnapshotStore,
    source_data: &SourceData,
    patched_files: Option<BTreeMap<String, String>>,
    architectures: &[alpm_types::SystemArchitecture],
) -> anyhow::Result<PackageInsertSpec> {
    let patch = match patched_files {
        Some(files) => build_patch_from_files(store, source_data, files).await?,
        None => None,
    };

    // Resolve dependencies/version off of the (possibly initial-patched)
    // source, so a patch supplied to fix an otherwise-unparseable PKGBUILD
    // (e.g. ogdf) is taken into account right away.
    let sourceinfo = store.sourceinfo(source_data, patch.as_deref()).await?;
    let deps = aurcache_deps::deps_from_srcinfo(&sourceinfo, architectures);
    let pkgbase = sourceinfo.base.name.to_string();

    Ok(PackageInsertSpec {
        pkgbase,
        version: sourceinfo.base.version.to_string(),
        deps: crate::pkg::DependencySet::of(&deps)?,
        pkgnames: deps.pkgnames,
        provides: deps.provides,
        patch,
        source_data: source_data.clone(),
    })
}

/// The package an add settled on.
pub(crate) struct AddedSource {
    pub(crate) pkgbase: String,
    /// It was tracked before this add, which then changed nothing but its
    /// source metadata.
    pub(crate) already_tracked: bool,
}

/// Add the package `package_spec` describes, unless it is already tracked.
///
/// Reporting pre-existence from here rather than probing before the call: the
/// check-then-act is one step inside the finalize, so the reported outcome
/// always matches the branch taken — a probe up front races a concurrent add
/// and can report `Added` for a row that already existed (or vice versa).
async fn finalize_package_add(
    services: &Services,
    context: &AddContext,
    package_spec: PackageInsertSpec,
) -> anyhow::Result<AddedSource> {
    let Services {
        client, store, db, ..
    } = services;
    if package_exists(db, &package_spec.pkgbase).await? {
        set_directly_requested(db, &package_spec.pkgbase).await?;
        // It may have been a dependency row that no version check has reached
        // yet, so it can still be missing its metadata.
        refresh_source_metadata(
            store,
            db,
            &services.activity,
            std::slice::from_ref(&package_spec.pkgbase),
        )
        .await;
        return Ok(AddedSource {
            pkgbase: package_spec.pkgbase,
            already_tracked: true,
        });
    }

    let mut visited: HashSet<String> = HashSet::from([package_spec.pkgbase.clone()]);
    let mut plan = AddPlan::default();
    let requested = package_spec.pkgbase.clone();

    // One snapshot for the whole plan: nothing is written until `persist_plan`,
    // so re-reading the packages table for each package planned would return
    // the same rows every time.
    let tracked = TrackedPackages::load(db).await?;
    plan_package_with_deps(
        &PlanContext {
            client,
            store,
            db,
            tracked: &tracked,
            context,
            activity: Some(&services.activity),
        },
        package_spec,
        &mut visited,
        &mut plan,
    )
    .await?;

    // Only the package the user asked for is directly requested; everything
    // else in the plan is a dependency pulled in on its behalf.
    let Some(root) = plan
        .packages
        .iter_mut()
        .find(|pkg| pkg.pkgbase == requested)
    else {
        return Err(anyhow!("Package add produced no inserted packages"));
    };
    root.directly_requested = true;

    let added_order = persist_plan(db, context, plan).await?;

    // Fill in the metadata now rather than waiting for the next scheduled
    // version check: the package route reads it straight from the row and has
    // no live fallback, so a package added between checks would otherwise show
    // no description or maintainer for up to an hour. Read from the checkouts
    // the plan just resolved, so nothing is fetched again.
    refresh_source_metadata(store, db, &services.activity, &added_order).await;

    trigger_initial_builds(
        db,
        &services.activity,
        context.platforms.as_slice(),
        &added_order,
    )
    .await?;
    Ok(AddedSource {
        pkgbase: requested,
        already_tracked: false,
    })
}

// Each argument is an independent input to the add flow (services, targeting,
// source, patches); bundling them into a struct would only move the same list.
pub async fn package_add(
    services: &Services,
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
    source_data: SourceData,
    patched_files: Option<BTreeMap<String, String>>,
) -> anyhow::Result<String> {
    let context = build_add_context(platforms, build_flags);
    let source_data = resolve_source_pkgbase(&services.client, source_data).await?;
    add_resolved_source(services, &context, source_data, patched_files)
        .await
        .map(|added| added.pkgbase)
}

async fn set_directly_requested(db: &DatabaseConnection, pkgbase: &str) -> anyhow::Result<()> {
    packages::Entity::update_many()
        .col_expr(packages::Column::DirectlyRequested, Expr::value(true))
        .filter(packages::Column::Name.eq(pkgbase))
        .exec(db)
        .await?;
    Ok(())
}

/// Turn a caller's source into one naming a pkgbase.
///
/// An AUR source may name any *pkgname*, which is not necessarily the pkgbase
/// that owns it (`czkawka-cli` belongs to `czkawka`), and everything downstream
/// keys on the pkgbase. Split out so a bulk add can resolve a whole batch of
/// names in one request rather than paying for this one package at a time --
/// see [`super::bulk_add`].
pub(crate) async fn resolve_source_pkgbase(
    client: &aurcache_deps::AurClient,
    source_data: SourceData,
) -> anyhow::Result<SourceData> {
    Ok(match source_data {
        SourceData::Aur { name } => SourceData::Aur {
            name: resolve_aur_pkgbase(client, &name).await?,
        },
        SourceData::Git { spec } => SourceData::Git { spec },
        SourceData::Upload { .. } => bail!("Upload sources are not yet supported"),
    })
}

/// Add a source whose pkgbase is already known.
///
/// The half of the add that stays per-package: a checkout, a dependency plan,
/// and the rows. Only the resolution in front of it batches.
///
/// Returns whether the package was already tracked (see
/// [`finalize_package_add`]).
pub(crate) async fn add_resolved_source(
    services: &Services,
    context: &AddContext,
    source_data: SourceData,
    patched_files: Option<BTreeMap<String, String>>,
) -> anyhow::Result<AddedSource> {
    let package_spec = resolve_srcinfo_to_spec(
        &services.store,
        &source_data,
        patched_files,
        &context.architectures(),
    )
    .await?;
    finalize_package_add(services, context, package_spec).await
}

#[allow(clippy::double_must_use)]
#[async_recursion]
async fn plan_dependency_recursive(
    plan_context: &PlanContext<'_>,
    pkgbase: &str,
    visited: &mut HashSet<String>,
    plan: &mut AddPlan,
) -> anyhow::Result<()> {
    let PlanContext {
        store, db, context, ..
    } = plan_context;
    // `visited` is the plan's name set: it already prevents planning the same
    // pkgbase twice, so the plan needs no separate "already planned?" lookup.
    if !visited.insert(pkgbase.to_string()) {
        return Ok(());
    }

    if package_exists(db, pkgbase).await? {
        return Ok(());
    }

    let source_data = SourceData::Aur {
        name: pkgbase.to_string(),
    };
    let package_spec =
        resolve_srcinfo_to_spec(store, &source_data, None, &context.architectures()).await?;
    plan_package_with_deps(plan_context, package_spec, visited, plan).await
}

/// Insert `pkgbase` and every AUR dependency it needs as dependency-only rows
/// of `dependent`, returning the pkgbases actually inserted.
///
/// Rows only: nothing is queued. The update flow builds what it inserted
/// through its own dependency readiness check; anything else wants
/// [`add_dependency`].
pub async fn insert_dependency(
    services: &Services,
    dependent: &packages::Model,
    pkgbase: &str,
) -> anyhow::Result<Vec<String>> {
    let Services {
        client, store, db, ..
    } = services;
    let context = AddContext::of(dependent);
    let mut visited = HashSet::new();
    let mut plan = AddPlan::default();
    let tracked = TrackedPackages::load(db).await?;
    plan_dependency_recursive(
        &PlanContext {
            client,
            store,
            db,
            tracked: &tracked,
            context: &context,
            activity: None,
        },
        pkgbase,
        &mut visited,
        &mut plan,
    )
    .await?;
    persist_plan(db, &context, plan).await
}

/// Add an AUR package as a dependency of `dependent`, and do everything an add
/// does with it:
/// the rows for it and its own dependencies, their metadata, and their initial
/// builds -- leaves queued, the rest waiting on them.
///
/// For a dependency chosen by hand, such as a replacement. Without the builds
/// the package would sit "Enqueued" with no build row for any worker to claim,
/// and whatever depends on it would wait on it for ever.
pub async fn add_dependency(
    services: &Services,
    dependent: &packages::Model,
    pkgbase: &str,
) -> anyhow::Result<()> {
    let added = insert_dependency(services, dependent, pkgbase).await?;
    refresh_source_metadata(&services.store, &services.db, &services.activity, &added).await;
    trigger_initial_builds(
        &services.db,
        &services.activity,
        AddContext::of(dependent).platforms.as_slice(),
        &added,
    )
    .await
}

/// Plan a package and, recursively, every AUR dependency it needs.
///
/// Writes nothing: results accumulate into `plan` so the whole graph can be
/// persisted in one transaction afterwards.
async fn plan_package_with_deps(
    plan_context: &PlanContext<'_>,
    package_spec: PackageInsertSpec,
    visited: &mut HashSet<String>,
    plan: &mut AddPlan,
) -> anyhow::Result<()> {
    let &PlanContext {
        client,
        tracked,
        activity,
        ..
    } = plan_context;
    // Packages planned earlier in this add are not in the database yet, so
    // they are offered alongside the rows that are; an add has no edges yet,
    // so there is nothing to prefer.
    let edges = resolve_edges(
        client,
        tracked,
        &Declaration {
            pkgbase: &package_spec.pkgbase,
            deps: &package_spec.deps,
            pkgnames: &package_spec.pkgnames,
            provides: &package_spec.provides,
        },
        &plan.candidates(),
        &HashSet::new(),
        activity,
    )
    .await?;

    for edge in edges {
        if edge.dependee == Dependee::Aur {
            plan_dependency_recursive(plan_context, &edge.pkgbase, visited, plan).await?;
        }
        plan.edges.push(PlannedEdge {
            dependent: package_spec.pkgbase.clone(),
            dependee: edge.pkgbase,
            version_constraint: crate::pkg::join_constraints(&edge.bounds),
        });
    }

    let split_packages = split_packages_json(&package_spec.pkgbase, &package_spec.pkgnames)?;
    let provides = provides_json(&package_spec.provides)?;

    // Pushed after its dependencies, keeping the plan dependency-first.
    plan.packages.push(PlannedPackage {
        pkgbase: package_spec.pkgbase,
        version: package_spec.version,
        source_data: package_spec.source_data,
        patch: package_spec.patch,
        split_packages,
        provides,
        directly_requested: false,
    });
    Ok(())
}

/// Write a planned add: every package and every edge, in one transaction.
///
/// Returns the inserted pkgbases in dependency-first order, for build
/// enqueueing.
async fn persist_plan(
    db: &DatabaseConnection,
    context: &AddContext,
    plan: AddPlan,
) -> anyhow::Result<Vec<String>> {
    let AddPlan { packages, edges } = plan;
    let txn = db.begin().await?;

    let mut ids: HashMap<String, i32> = HashMap::new();
    let mut added_order: Vec<String> = Vec::with_capacity(packages.len());
    for pkg in packages {
        let model = packages::ActiveModel {
            // `name` stores the pkgbase; this codebase keeps one row per package
            // base and tracks split package names separately.
            name: Set(pkg.pkgbase.clone()),
            status: Set(BuildState::Enqueued),
            upstream_version: Set(Some(pkg.version)),
            platforms: Set(context.platforms.clone()),
            build_flags: Set(context.build_flags.clone()),
            source_data: Set(pkg.source_data),
            directly_requested: Set(pkg.directly_requested),
            split_packages: Set(pkg.split_packages),
            provides: Set(pkg.provides),
            patch: Set(pkg.patch),
            ..Default::default()
        };
        let inserted = model.insert(&txn).await?;
        ids.insert(pkg.pkgbase.clone(), inserted.id);
        added_order.push(pkg.pkgbase);
    }

    // Edges may point at packages that already existed; resolve those once.
    let existing: Vec<&str> = edges
        .iter()
        .map(|edge| edge.dependee.as_str())
        .filter(|name| !ids.contains_key(*name))
        .collect();
    if !existing.is_empty() {
        for pkg in Packages::find()
            .filter(packages::Column::Name.is_in(existing))
            .all(&txn)
            .await?
        {
            ids.insert(pkg.name, pkg.id);
        }
    }

    for edge in edges {
        // A dependee that is neither planned nor already present has nothing to
        // link to; the dependent still builds against the official repos.
        let (Some(dependent_id), Some(dependee_id)) =
            (ids.get(&edge.dependent), ids.get(&edge.dependee))
        else {
            continue;
        };
        aurcache_db::dependencies::ActiveModel {
            dependent_id: Set(*dependent_id),
            dependee_id: Set(*dependee_id),
            version_constraint: Set(edge.version_constraint),
            ..Default::default()
        }
        .save(&txn)
        .await?;
    }

    txn.commit().await?;
    Ok(added_order)
}

pub(crate) fn split_packages_json(
    pkgbase: &str,
    pkgnames: &[String],
) -> anyhow::Result<Option<String>> {
    if pkgnames.len() <= 1 && pkgnames.first().is_none_or(|name| name == pkgbase) {
        return Ok(None);
    }

    Ok(Some(serde_json::to_string(pkgnames)?))
}

pub(crate) fn provides_json(provides: &[String]) -> anyhow::Result<Option<String>> {
    if provides.is_empty() {
        return Ok(None);
    }

    Ok(Some(serde_json::to_string(provides)?))
}
