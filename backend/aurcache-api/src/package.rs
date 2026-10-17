use crate::models::authenticated::Authenticated;
use crate::models::operations::ActiveOperation;
use crate::models::package::{
    AddPackage, PackagePatch, SourceFileContent, SourceFileList, SourceFileUpdate,
    SourcePreviewFileRequest, SourcePreviewRequest, UpdatePackage,
};
use crate::models::package::{
    AddPackages, AurNotFoundPackage, AurPackage, BulkAddAccepted, BulkAddEntry, BulkAddOutcome,
    BulkAddProgress, ExtendedPackage, PackageDependency, PackageFile, PackageSource, SimplePackage,
};
use crate::models::package::{DependencyOptions, ReplaceDependency};
use crate::utils::error::{ApiError, err};
use crate::utils::operation::{self, Counts, Recorder};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, QueueCause};
use aurcache_common::build_state::{BuildState, BuildTrigger};
use aurcache_db::helpers::builds::{
    latest_successful_version_any_platform, latest_successful_version_expr,
};
use aurcache_db::helpers::files::total_artifact_size_expr;
use aurcache_db::helpers::operations;
use aurcache_db::lists::{BuildFlags, Platforms};
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::{Dependencies, Files, Packages};
use aurcache_db::{dependencies, files, packages};
use aurcache_utils::package::add::package_add;
use aurcache_utils::package::live_check::package_remove;
use aurcache_utils::package::replace::{self, ReplaceError};
use aurcache_utils::package::update::{
    Dispatch, package_resync_dependencies, package_update, queued,
};
use aurcache_utils::patch::SourcePatch;
use aurcache_utils::pkg::satisfies_constraint;
use aurcache_utils::services::Services;
use aurcache_utils::snapshot::SnapshotStore;
use pacman_mirrors::platforms::Platform;
use rocket::http::Status;
use rocket::response::status;
use sea_orm::FromQueryResult;

use rocket::serde::json::Json;
use rocket::{State, delete, get, patch, post, put};
use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::{ActiveModelTrait, DatabaseConnection, JoinType, Order, Select};
use sea_orm::{
    ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, QueryTrait, RelationTrait,
};
use std::str::FromStr;
use std::sync::Arc;
use utoipa::OpenApi;

/// The row id of the package named `pkgbase`.
pub(crate) async fn package_id(db: &DatabaseConnection, pkgbase: &str) -> Result<i32, ApiError> {
    Ok(package_by_pkgbase(db, pkgbase).await?.id)
}

/// Resolve a package by its pkgbase, the public identifier.
///
/// Row ids are deliberately absent from the public API — they are an
/// implementation detail. The two also cannot be accepted interchangeably:
/// some real pkgbases are entirely numeric (`1337` and `67` both exist in the
/// AUR), so a route taking either would resolve those names to whatever rows
/// happened to hold those ids — a wrong-package bug rather than a not-found
/// error. Hence no id fallback.
async fn package_by_pkgbase(
    db: &DatabaseConnection,
    pkgbase: &str,
) -> Result<packages::Model, ApiError> {
    Packages::find()
        .filter(packages::Column::Name.eq(pkgbase))
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, format!("no package '{pkgbase}'")))
}

#[derive(OpenApi)]
#[openapi(paths(
    package_add_endpoint,
    packages_add_endpoint,
    active_operations,
    bulk_add_progress,
    package_update_entity_endpoint,
    package_update_endpoint,
    package_del,
    package_dependency_options,
    package_dependency_replace,
    package_list,
    get_package,
    package_source_files,
    package_source_file,
    package_source_file_update,
    package_source_preview_files,
    package_source_preview_file
))]
pub struct PackageApi;

/// Parse the platform names a request carried, if any.
fn parse_platforms(platforms: Option<Vec<String>>) -> Result<Option<Vec<Platform>>, ApiError> {
    platforms
        .map(|v| {
            // `ParsePlatformError` names the rejected value (`unknown
            // platform 'x'`), which a fixed string could not.
            v.into_iter()
                .map(|s| Platform::from_str(&s))
                .collect::<Result<Vec<Platform>, _>>()
                .map_err(|e| err(Status::BadRequest, e))
        })
        .transpose()
}

/// Counts a bulk add's entries, and logs each package it added.
struct BulkAddRecorder {
    activity: ActivityLog,
    username: Option<String>,
    counts: Counts,
}

impl Recorder<BulkAddEntry> for BulkAddRecorder {
    fn record(&mut self, entry: &BulkAddEntry) -> Counts {
        match &entry.outcome {
            BulkAddOutcome::Failed { .. } => self.counts.failed += 1,
            BulkAddOutcome::Added => {
                self.counts.completed += 1;
                // The pkgbase it landed under, which is what the link opens;
                // the name as typed where there is none.
                self.activity.emit_by(
                    Event::PackageAdded {
                        pkg: entry
                            .pkgbase
                            .clone()
                            .unwrap_or_else(|| entry.name.clone())
                            .into(),
                    },
                    self.username.clone(),
                );
            }
            BulkAddOutcome::Existed => self.counts.completed += 1,
        }
        self.counts
    }
}

#[utoipa::path(
    responses(
            (status = 202, description = "Bulk add started", body = BulkAddAccepted),
            (status = 400, description = "Invalid platform name, or no sources given"),
    )
)]
/// Add many packages, returning before any of them are added.
///
/// Adding a thousand packages is minutes of `git` checkouts, so this starts the
/// work and hands back an id rather than holding the request open. The job does
/// not depend on the caller staying: closing the connection leaves it running,
/// and its progress -- including anything that failed while nobody was
/// watching -- is read back from [`bulk_add_progress`].
#[post("/packages", data = "<input>")]
pub async fn packages_add_endpoint(
    services: &State<Services>,
    input: Json<AddPackages>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<status::Accepted<Json<BulkAddAccepted>>, ApiError> {
    let input = input.into_inner();
    if input.sources.is_empty() {
        return Err(err(Status::BadRequest, "No sources given"));
    }
    let platforms = parse_platforms(input.platforms)?;
    let build_flags = input.build_flags;
    let total = i32::try_from(input.sources.len()).unwrap_or(i32::MAX);

    let job_id = operations::create(&services.db, operations::KIND_BULK_ADD, total)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    let services_task = services.inner().clone();
    operation::spawn(
        services.db.clone(),
        al.inner().clone(),
        job_id,
        operations::KIND_BULK_ADD,
        move |progress| async move {
            aurcache_utils::package::bulk_add::bulk_add(
                &services_task,
                platforms,
                build_flags,
                input.sources,
                progress,
            )
            .await;
        },
        BulkAddRecorder {
            activity: al.inner().clone(),
            username: a.username,
            counts: Counts::default(),
        },
    );

    Ok(status::Accepted(Json(BulkAddAccepted {
        job_id,
        accepted: total,
    })))
}
/// Every long-running operation still in flight.
///
/// A bulk add or a restore outlives the request that started it and the page
/// that was watching, so without this there is no way back to one: a browser
/// that reloaded, navigated away, or never started the job has no id to poll.
/// Listing them is what lets a job be found again and re-attached to.
///
/// Counters only. The entries live behind the per-job endpoints, which is where
/// a caller that has decided to watch one goes; sending every log to everyone
/// listing what is running would be the expensive part of a cheap request.
#[utoipa::path(
    responses((status = 200, description = "Operations still running", body = Vec<ActiveOperation>)),
)]
#[get("/operations")]
pub async fn active_operations(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<Vec<ActiveOperation>>, ApiError> {
    let running = operations::active(db.inner())
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    Ok(Json(running))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Progress of a bulk add", body = BulkAddProgress),
            (status = 404, description = "No such job"),
    ),
    params(
        ("id", description = "Job id returned when the bulk add was started"),
        ("after", description = "How many entries the caller already holds"),
    )
)]
/// Read a bulk add's progress from `after` onwards.
///
/// Offset-based rather than a stream, for the same reason build output is: an
/// observer can attach late and still see everything from the beginning, and
/// one that disconnects misses nothing, because the record is what the job
/// writes to rather than a side effect of someone watching.
#[get("/packages/bulk/<id>?<after>")]
pub async fn bulk_add_progress(
    db: &State<DatabaseConnection>,
    id: i32,
    after: Option<usize>,
    _a: Authenticated,
) -> Result<Json<BulkAddProgress>, ApiError> {
    let job = operations::get(db.inner(), id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .filter(|job| job.kind == operations::KIND_BULK_ADD)
        .ok_or_else(|| err(Status::NotFound, format!("no bulk add {id}")))?;

    let entries = operations::entries_after(&job.log, after.unwrap_or(0));

    Ok(Json(BulkAddProgress {
        id: job.id,
        total: job.total,
        completed: job.completed,
        failed: job.failed,
        finished: job.finished_at.is_some(),
        entries,
    }))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Add new Package"),
    )
)]
#[post("/package", data = "<input>")]
pub async fn package_add_endpoint(
    services: &State<Services>,
    input: Json<AddPackage>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<(), ApiError> {
    let input = input.into_inner();
    let platforms = parse_platforms(input.platforms)?;

    let new_pkg_name = package_add(
        services,
        platforms,
        input.build_flags,
        input.source,
        input.patched_files,
    )
    .await
    // Adding is driven by user input: an unknown AUR name, an unreachable git
    // remote or an unparseable PKGBUILD are all "this request cannot be
    // fulfilled" rather than a server fault, and the flow reports them as an
    // untyped `anyhow` error we cannot tell apart from an internal one.
    .map_err(|e| err(Status::BadRequest, e))?;

    al.emit_by(
        Event::PackageAdded {
            pkg: new_pkg_name.into(),
        },
        a.username,
    );
    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Update parts of package"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[patch("/package/<pkgbase>", data = "<input>")]
pub async fn package_update_entity_endpoint(
    services: &State<Services>,
    input: Json<PackagePatch>,
    pkgbase: &str,
    a: Authenticated,
) -> Result<(), ApiError> {
    let db = &services.db;

    // We cannot move things out of Json<T>, but we can move it out of T.
    let input = input.into_inner();
    let patch_changed = input.patch.is_some();
    // What the request touched, for the log, named as a reader would.
    let fields: Vec<String> = [
        (input.build_flags.is_some(), "build flags"),
        (input.platforms.is_some(), "platforms"),
        (patch_changed, "source patch"),
    ]
    .into_iter()
    .filter(|(touched, _)| *touched)
    .map(|(_, field)| field.to_string())
    .collect();
    let pkg = package_by_pkgbase(db, pkgbase).await?;

    // Dependencies are read per architecture — a PKGBUILD can declare
    // `depends_aarch64` separately — and the graph is the union across the
    // platforms a package is built for. Changing that set therefore changes
    // which dependencies are required, so it needs the same resync a patch
    // gets. Compared as sets, so a no-op write (including the same set in a
    // different order) does not trigger a needless source checkout. Validated like the add endpoints: storing an
    // unknown name would fail every later build that reads the set.
    let requested_platforms = parse_platforms(input.platforms.clone())?.map(Platforms::new);
    // Stored as a `SourcePatch`; a value that is not one would fail every
    // later read of the package's source.
    if let Some(Some(patch)) = &input.patch {
        SourcePatch::parse(patch).map_err(|e| err(Status::BadRequest, e))?;
    }
    let platforms_changed = requested_platforms
        .as_ref()
        .is_some_and(|requested| *requested != pkg.platforms);

    // Start building the update operation
    let update_pkg = packages::ActiveModel {
        id: Set(pkg.id),
        build_flags: input
            .build_flags
            .map_or(NotSet, |flags| Set(BuildFlags::new(flags))),
        platforms: requested_platforms.map_or(NotSet, Set),
        patch: input.patch.map_or(NotSet, Set),
        // Everything else is `NotSet`, left untouched, so a column added
        // later cannot be cleared by a patch that never mentions it. The
        // mirrored AUR metadata in particular belongs to the version-check
        // scheduler.
        ..Default::default()
    };

    // Execute the update query
    let updated = update_pkg
        .update(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    if !fields.is_empty() {
        services.activity.emit_by(
            Event::PackageChanged {
                pkg: updated.name.as_str().into(),
                fields,
            },
            a.username,
        );
    }

    // A patch being set or cleared here (e.g. via the "reset patch" action)
    // can change `depends`/`makedepends` without bumping the package's
    // version, and so can a change to the platform set, so keep the dependency
    // graph in sync immediately. `package_resync_dependencies` recomputes the
    // whole graph from source, which both adds newly-required dependencies and
    // drops ones no longer needed.
    if patch_changed || platforms_changed {
        package_resync_dependencies(services, &updated)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?;
    }

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "List the files in a package's source, available for viewing/editing", body = SourceFileList),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[get("/package/<pkgbase>/source/files")]
pub async fn package_source_files(
    db: &State<DatabaseConnection>,
    store: &State<Arc<SnapshotStore>>,
    pkgbase: &str,
    _a: Authenticated,
) -> Result<Json<SourceFileList>, ApiError> {
    let db = db.inner();

    let pkg = package_by_pkgbase(db, pkgbase).await?;

    let files = store
        .list_files(&pkg.source_data)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    // A patch that no longer parses marks nothing here; opening the file is
    // where that is reported.
    let patched = pkg
        .patch
        .as_deref()
        .and_then(|raw| SourcePatch::parse(raw).ok())
        .map(|patch| patch.paths().map(str::to_string).collect())
        .unwrap_or_default();

    Ok(Json(SourceFileList { files, patched }))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get the pristine content, and (if applicable) patched content, of a source file", body = SourceFileContent),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("path", description = "File path relative to the source root, e.g. 'PKGBUILD'")
    )
)]
#[get("/package/<pkgbase>/source/file?<path>")]
pub async fn package_source_file(
    db: &State<DatabaseConnection>,
    store: &State<Arc<SnapshotStore>>,
    pkgbase: &str,
    path: String,
    _a: Authenticated,
) -> Result<Json<SourceFileContent>, ApiError> {
    let db = db.inner();

    let pkg = package_by_pkgbase(db, pkgbase).await?;

    store
        .read_file_with_patch_status(&pkg.source_data, pkg.patch.as_deref(), &path)
        .await
        .map(Json)
        .map_err(|e| err(Status::NotFound, e))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Save an edit to a source file as part of the package's patch"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[put("/package/<pkgbase>/source/file", data = "<input>")]
pub async fn package_source_file_update(
    services: &State<Services>,
    pkgbase: &str,
    input: Json<SourceFileUpdate>,
    a: Authenticated,
) -> Result<(), ApiError> {
    let db = &services.db;
    let input = input.into_inner();

    let pkg = package_by_pkgbase(db, pkgbase).await?;

    // Diff against the pristine (unpatched) file, not the currently effective
    // one, so re-saving the same edit twice is idempotent.
    let original = services
        .store
        .read_file(&pkg.source_data, None, &input.path)
        .await
        .map_err(|e| err(Status::NotFound, e))?;

    let mut patch = pkg
        .patch
        .as_deref()
        .map(SourcePatch::parse)
        .transpose()
        .map_err(|e| err(Status::InternalServerError, e))?
        .unwrap_or_default();
    patch.merge_file(&input.path, &original, &input.content);

    let new_patch = patch
        .stored()
        .map_err(|e| err(Status::InternalServerError, e))?;

    // No need to validate the patch applies cleanly here: it was just
    // diffed fresh against the current pristine content above, so applying
    // it back is guaranteed to succeed.
    let update_pkg = packages::ActiveModel {
        id: Set(pkg.id),
        patch: Set(new_patch.clone()),
        ..Default::default()
    };
    update_pkg
        .update(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    services.activity.emit_by(
        Event::SourceEdited {
            pkg: pkg.name.as_str().into(),
            path: input.path.clone(),
        },
        a.username,
    );

    // A patch can change `depends`/`makedepends` without bumping the
    // package's version, so resync the dependency graph immediately rather
    // than waiting for the next explicit "update" trigger.
    let mut resynced_pkg = pkg;
    resynced_pkg.patch = new_patch;
    package_resync_dependencies(services, &resynced_pkg)
        .await
        .map_err(|e| {
            err(
                Status::InternalServerError,
                format!("Patch saved, but failed to resync dependencies: {e}"),
            )
        })?;

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "List source files for a not-yet-added source", body = SourceFileList),
    )
)]
#[post("/package/source/preview/files", data = "<input>")]
pub async fn package_source_preview_files(
    store: &State<Arc<SnapshotStore>>,
    input: Json<SourcePreviewRequest>,
    _a: Authenticated,
) -> Result<Json<SourceFileList>, ApiError> {
    let files = store
        .list_files(&input.source)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    Ok(Json(SourceFileList {
        files,
        patched: Vec::new(),
    }))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get the pristine content of a source file for a not-yet-added source", body = SourceFileContent),
    )
)]
#[post("/package/source/preview/file", data = "<input>")]
pub async fn package_source_preview_file(
    store: &State<Arc<SnapshotStore>>,
    input: Json<SourcePreviewFileRequest>,
    _a: Authenticated,
) -> Result<Json<SourceFileContent>, ApiError> {
    let input = input.into_inner();
    let original_content = store
        .read_file(&input.source, None, &input.path)
        .await
        .map_err(|e| err(Status::NotFound, e))?;

    Ok(Json(SourceFileContent {
        path: input.path,
        original_content,
        patched_content: None,
        patch_error: None,
        stored_patch: None,
    }))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Update package to newest AUR version"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[post("/package/<pkgbase>/update", data = "<input>")]
pub async fn package_update_endpoint(
    services: &State<Services>,
    pkgbase: &str,
    input: Json<UpdatePackage>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<Json<Vec<i32>>, ApiError> {
    let db = &services.db;

    let pkg_model: packages::Model = package_by_pkgbase(db, pkgbase).await?;
    let package_name = pkg_model.name.clone();
    let forced = input.force;
    // What the request was, named by what came before it: without `force` it
    // is an update, and with it a rebuild of a build that worked or a retry of
    // one that failed.
    let cause = if forced {
        QueueCause::after(pkg_model.status == BuildState::Failed)
    } else {
        QueueCause::Update
    };

    // An operator's Update or Rebuild, from the UI or the CLI.
    let results = package_update(services, pkg_model, forced, BuildTrigger::User)
        .await
        // Same as adding: "already up to date", an unresolvable source, or a
        // patch that no longer applies are all caller-visible conditions.
        .map_err(|e| err(Status::BadRequest, e))?;

    al.emit_by(
        queued(&package_name, cause, &results, None, None),
        a.username,
    );
    Ok(Json(
        results
            .into_iter()
            .filter(|r| r.dispatch == Dispatch::Enqueued)
            .map(|r| r.build_number)
            .collect(),
    ))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Remove direct request flag from package and live-check it"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[delete("/package/<pkgbase>")]
pub async fn package_del(
    services: &State<Services>,
    pkgbase: &str,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<(), ApiError> {
    let db = &services.db;

    // query this before removing package ownership!
    let pkg = package_by_pkgbase(db, pkgbase).await?;

    // Deletes the package -- checkout included -- only if nothing depends on
    // it; otherwise it merely stops being requested and keeps everything.
    package_remove(db, &services.store, &services.repo, pkg.id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    al.emit_by(
        Event::PackageDeleted {
            pkg: pkg.name.into(),
        },
        a.username,
    );

    Ok(())
}
#[utoipa::path(
    responses(
            (status = 200, description = "List of all packages", body = [SimplePackage]),
    ),
    params(
            ("limit", description = "limit of packages"),
            ("page", description = "page of packages"),
            ("dependencies", description = "include packages that are only here as dependencies")
    )
)]
#[get("/packages/list?<limit>&<page>&<dependencies>")]
pub async fn package_list(
    db: &State<DatabaseConnection>,
    limit: Option<u64>,
    page: Option<u64>,
    dependencies: Option<bool>,
    _a: Authenticated,
) -> Result<Json<Vec<SimplePackage>>, ApiError> {
    let db = db.inner();

    list_packages(db, limit, page, dependencies.unwrap_or(false))
        .await
        .map(Json)
        .map_err(|e| err(Status::InternalServerError, e))
}

/// The package-list projection shared by the list and dashboard routes.
///
/// Every listed package has the same fields, so there is one place the shape
/// of a [`SimplePackage`] is described.
pub(crate) fn package_row_select() -> Select<Packages> {
    Packages::find()
        .select_only()
        .column(packages::Column::Name)
        .column(packages::Column::Status)
        .column(packages::Column::OutOfDate)
        .column(packages::Column::UpstreamVersion)
        .column(packages::Column::DirectlyRequested)
        // No COALESCE to an empty string: a package with no build has no
        // version, and `null` says that where `""` is indistinguishable from a
        // build that produced a blank one.
        .column_as(latest_successful_version_expr(), "built_version")
        .column_as(total_artifact_size_expr(), "total_size")
}

/// List packages, by default only the ones somebody asked for.
///
/// `dependencies` opts into the rest. It defaults to off because that is what
/// every existing caller means by "the packages": a repository's dependency
/// closure is usually the larger part of it, and reading the list as the set of
/// things being maintained is the common case.
async fn list_packages(
    db: &DatabaseConnection,
    limit: Option<u64>,
    page: Option<u64>,
    dependencies: bool,
) -> Result<Vec<SimplePackage>, sea_orm::DbErr> {
    // Both correlated subqueries live in `package_row_select`, so they report
    // per row without a query per package. The "successful builds only" rule
    // matters: taking the newest attempt regardless of outcome meant a failed
    // build of a new version reported that version as built, and the version
    // check then compared upstream against it and cleared the out-of-date
    // flag — so an upstream release whose first build failed stopped being
    // flagged at all.

    let all: Vec<SimplePackage> = package_row_select()
        .apply_if((!dependencies).then_some(()), |query, ()| {
            query.filter(packages::Column::DirectlyRequested.eq(true))
        })
        .order_by(packages::Column::OutOfDate, Order::Desc)
        .order_by(packages::Column::Id, Order::Desc)
        .limit(limit)
        .offset(crate::utils::pagination::page_offset(page, limit))
        .into_model::<SimplePackage>()
        .all(db)
        .await?;

    Ok(all)
}

async fn list_package_relations(
    db: &DatabaseConnection,
    pkg_id: i32,
    direction: RelationDirection,
) -> Result<Vec<PackageDependency>, sea_orm::DbErr> {
    let (filter_col, relation) = match direction {
        RelationDirection::Dependencies => (
            dependencies::Column::DependentId,
            dependencies::Relation::Dependee.def(),
        ),
        RelationDirection::Dependents => (
            dependencies::Column::DependeeId,
            dependencies::Relation::Dependent.def(),
        ),
    };

    let rows = Dependencies::find()
        .select_only()
        .column_as(packages::Column::Name, "name")
        .column(dependencies::Column::VersionConstraint)
        .column_as(packages::Column::Status, "status")
        // The repository version of the joined package, on the same rule.
        .column_as(latest_successful_version_expr(), "built_version")
        .join(JoinType::InnerJoin, relation)
        .filter(filter_col.eq(pkg_id))
        .order_by_asc(dependencies::Column::Id)
        .into_model::<DependencyRow>()
        .all(db)
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            // Deliberately the same rule the builder applies when it decides
            // whether to promote a dependent, so the page cannot disagree with
            // what the queue actually does. Unlike the builder this is not
            // scoped to one platform: the page is not either, and a package
            // built for several reports the newest success across them.
            let satisfied = row
                .built_version
                .as_deref()
                .is_some_and(|built| satisfies_constraint(built, &row.version_constraint));
            PackageDependency {
                name: row.name,
                version_constraint: row.version_constraint,
                status: row.status,
                built_version: row.built_version,
                satisfied,
            }
        })
        .collect())
}

/// The columns behind [`PackageDependency`]; `satisfied` is derived, not stored.
#[derive(FromQueryResult)]
struct DependencyRow {
    name: String,
    version_constraint: String,
    status: BuildState,
    built_version: Option<String>,
}

#[derive(Copy, Clone)]
enum RelationDirection {
    Dependencies,
    Dependents,
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get package details, read from the database alone", body = ExtendedPackage),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[get("/package/<pkgbase>")]
pub async fn get_package(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    _a: Authenticated,
) -> Result<Json<ExtendedPackage>, ApiError> {
    let db = db.inner();

    let pkg = package_by_pkgbase(db, pkgbase).await?;

    // Independent reads over one pooled connection: serial awaits would pay
    // each round trip in turn for queries that share only the package id.
    let (built_version, dependencies, dependents, files) = tokio::join!(
        latest_successful_version_any_platform(db, pkg.id),
        list_package_relations(db, pkg.id, RelationDirection::Dependencies),
        list_package_relations(db, pkg.id, RelationDirection::Dependents),
        package_files(db, pkg.id),
    );
    let built_version = built_version.map_err(|e| err(Status::InternalServerError, e))?;
    let dependencies = dependencies.map_err(|e| err(Status::InternalServerError, e))?;
    let dependents = dependents.map_err(|e| err(Status::InternalServerError, e))?;
    let files = files.map_err(|e| err(Status::InternalServerError, e))?;

    let has_patch = pkg.patch.is_some();

    let package_source = package_source(&pkg)?;

    // Borrowed: the stored string is not used after this, only the parse.
    let split_packages: Option<Vec<String>> = pkg
        .split_packages
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());

    let ext_pkg = ExtendedPackage {
        // Mirrored from the package's checkout, so a git-sourced package
        // describes itself as fully as an AUR one.
        description: pkg.source_description.clone(),
        project_url: pkg.source_project_url.clone(),
        licenses: pkg.source_licenses.clone(),
        maintainer: pkg.source_maintainer.clone(),
        first_submitted: pkg.source_first_submitted,
        last_modified: pkg.source_last_modified,
        name: pkg.name,
        directly_requested: pkg.directly_requested,
        status: pkg.status,
        out_of_date: pkg.out_of_date,
        built_version,
        package_source,
        selected_platforms: pkg.platforms.names(),
        selected_build_flags: Some(pkg.build_flags.as_slice().to_vec()),
        // How current this is depends on the version-check interval; `None`
        // means no check has run yet.
        upstream_version: pkg.upstream_version,
        split_packages,
        files,
        dependencies,
        dependents,
        has_patch,
    };

    Ok(Json(ext_pkg))
}

/// The `PackageSource` for a package row, resolved entirely from persisted
/// fields — no AUR lookup happens on this path. The version-check scheduler
/// mirrors what it needs onto the row, and `package::add` fills it in
/// immediately; the AUR call this used to make was ~128ms of a ~130ms response
/// and one of the AUR's 4000 daily calls per page view.
fn package_source(pkg: &packages::Model) -> Result<PackageSource, ApiError> {
    match &pkg.source_data {
        // The last check did not find it in the AUR. Its page still renders --
        // the metadata comes from the checkout -- but it says the package is
        // gone from upstream.
        SourceData::Aur { .. } if pkg.aur_missing == Some(true) => {
            Ok(PackageSource::AurNotFound(AurNotFoundPackage {}))
        }
        SourceData::Aur { .. } => Ok(PackageSource::Aur(aur_source(pkg))),
        SourceData::Git { spec } => Ok(PackageSource::Git(spec.clone())),
        SourceData::Upload { .. } => Err(err(
            Status::NotImplemented,
            "Upload sources are not yet supported",
        )),
    }
}

/// The artifacts currently in the repository for a package.
///
/// Read straight from the `files` rows rather than by listing the repository
/// directory: those rows are what publishing and the delete path both maintain,
/// so this is the same list the server acts on, and the size comes with them
/// instead of costing a `stat` per artifact per page view.
///
/// Ordered by filename so the page is stable across requests; a split package
/// otherwise lists its parts in whatever order the rows came back in.
async fn package_files(
    db: &DatabaseConnection,
    pkg_id: i32,
) -> Result<Vec<PackageFile>, sea_orm::DbErr> {
    Ok(Files::find()
        .filter(files::Column::PackageId.eq(pkg_id))
        .order_by_asc(files::Column::Filename)
        .all(db)
        .await?
        .into_iter()
        .map(|f| PackageFile {
            filename: f.filename,
            platform: f.platform.to_string(),
            size: f.size,
        })
        .collect())
}

/// The AUR page for a pkgbase. Derived, never fetched.
fn aur_pkgbase_url(pkgbase: &str) -> String {
    format!("https://aur.archlinux.org/pkgbase/{pkgbase}")
}

/// The AUR-specific part of a package's source description.
///
/// Everything else a package page shows comes from its checkout — see
/// `aurcache_utils::package::metadata` — so this is only the flag the AUR
/// alone knows and the link back to it.
fn aur_source(pkg: &packages::Model) -> AurPackage {
    AurPackage {
        name: pkg.name.clone(),
        aur_flagged_outdated: pkg.aur_flagged_outdated.unwrap_or(false),
        aur_url: aur_pkgbase_url(&pkg.name),
    }
}

#[cfg(test)]
mod tests {
    use super::{RelationDirection, list_package_relations, list_packages};
    use aurcache_common::build_state::BuildState;
    use aurcache_db::migration::Migrator;
    use aurcache_db::packages::SourceData;
    use aurcache_db::{dependencies, packages};
    use sea_orm::{ActiveModelTrait, Database, Set, TryIntoModel};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn package_list_hides_dependencies_unless_asked() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        packages::ActiveModel {
            name: Set("visible-package".to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: "visible-package".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        packages::ActiveModel {
            name: Set("hidden-dependency".to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: "hidden-dependency".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let packages = list_packages(&db, None, None, false).await.unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "visible-package");
        assert!(packages[0].directly_requested);

        // Opting in returns both, and each says which kind it is -- the list
        // is only worth widening if the two can still be told apart.
        let mut packages = list_packages(&db, None, None, true).await.unwrap();
        packages.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "hidden-dependency");
        assert!(!packages[0].directly_requested);
        assert_eq!(packages[1].name, "visible-package");
        assert!(packages[1].directly_requested);
    }

    #[tokio::test]
    async fn package_dependencies_include_link_targets() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("2.0.0".to_string())),
            build_flags: Set("--noconfirm".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: "child".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=2.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let deps = list_package_relations(&db, parent.id, RelationDirection::Dependencies)
            .await
            .unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "child");
        assert_eq!(deps[0].version_constraint, ">=2.0");
    }

    #[tokio::test]
    async fn package_dependents_include_reverse_link_targets() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let dependency = packages::ActiveModel {
            name: Set("dependency".to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: "dependency".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("2.0.0".to_string())),
            build_flags: Set("--noconfirm".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(dependency.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let dependents = list_package_relations(&db, dependency.id, RelationDirection::Dependents)
            .await
            .unwrap();

        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].name, "parent");
        assert_eq!(dependents[0].version_constraint, ">=1.0");
    }
}

#[cfg(test)]
mod file_tests {
    use super::package_files;
    use aurcache_common::build_state::BuildState;
    use aurcache_db::files;
    use aurcache_db::migration::Migrator;
    use pacman_mirrors::platforms::Platform;
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Database, DatabaseConnection, Set};
    use sea_orm_migration::MigratorTrait;

    /// A migrated database with the packages these tests hang files off.
    ///
    /// The packages have to exist: `files.package_id` references them, and
    /// SQLite enforces that here because sqlx opens connections with
    /// `foreign_keys` on.
    async fn db_with_packages(ids: &[i32]) -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        for id in ids {
            db.execute_unprepared(&format!(
                "INSERT INTO packages \
                 (id, name, status, out_of_date, build_flags, platforms, source_data, directly_requested) \
                 VALUES ({id}, 'p{id}', 0, 0, '', 'x86_64', '{{\"type\":\"aur\",\"name\":\"p{id}\"}}', 1)"
            ))
            .await
            .unwrap();
        }
        db
    }

    async fn file(db: &DatabaseConnection, name: &str, pkg_id: i32, size: Option<i64>) {
        files::ActiveModel {
            filename: Set(name.to_string()),
            platform: Set(Platform::X86_64),
            package_id: Set(pkg_id),
            size: Set(size),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    /// The page lists one row per artifact -- a split package has several --
    /// scoped to the package that owns them.
    #[tokio::test]
    async fn lists_only_this_package_s_artifacts() {
        let db = db_with_packages(&[1, 2]).await;

        file(&db, "hello-1.0-1-x86_64.pkg.tar.zst", 1, Some(1000)).await;
        file(&db, "hello-docs-1.0-1-x86_64.pkg.tar.zst", 1, Some(24)).await;
        file(&db, "other-1.0-1-x86_64.pkg.tar.zst", 2, Some(99)).await;

        let listed = package_files(&db, 1).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed.iter().map(|f| f.size).sum::<Option<i64>>(),
            Some(1024)
        );
        assert!(listed.iter().all(|f| f.platform == "x86_64"));
    }

    /// Ordered by filename, so a split package does not reshuffle its parts
    /// between two loads of the same page.
    #[tokio::test]
    async fn artifacts_come_back_in_a_stable_order() {
        let db = db_with_packages(&[1]).await;

        file(&db, "zzz-1.0-1-x86_64.pkg.tar.zst", 1, Some(1)).await;
        file(&db, "aaa-1.0-1-x86_64.pkg.tar.zst", 1, Some(2)).await;

        let listed = package_files(&db, 1).await.unwrap();
        let names: Vec<&str> = listed.iter().map(|f| f.filename.as_str()).collect();
        assert_eq!(
            names,
            [
                "aaa-1.0-1-x86_64.pkg.tar.zst",
                "zzz-1.0-1-x86_64.pkg.tar.zst"
            ]
        );
    }

    /// The list column totals a package's artifacts, and applies the same
    /// all-or-nothing rule the detail page does: one unrecorded size makes the
    /// whole total unknown, rather than reporting a sum smaller than the files
    /// it covers. Exercised through the real SQL, since the rule lives in a
    /// `CASE` expression rather than in Rust.
    #[tokio::test]
    async fn the_list_totals_artifact_sizes_all_or_nothing() {
        use super::list_packages;
        use aurcache_db::packages;
        use aurcache_db::packages::SourceData;

        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let mut ids = std::collections::HashMap::new();
        for name in ["complete", "partial", "unbuilt"] {
            let row = packages::ActiveModel {
                name: Set(name.to_string()),
                status: Set(BuildState::Successful),
                out_of_date: Set(false),
                upstream_version: Set(Some("1.0.0".to_string())),
                build_flags: Set(Default::default()),
                platforms: Set("x86_64".parse().unwrap()),
                source_data: Set(SourceData::Aur { name: name.into() }),
                directly_requested: Set(true),
                split_packages: Set(None),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
            ids.insert(name, row.id);
        }

        file(&db, "complete-a.pkg.tar.zst", ids["complete"], Some(1000)).await;
        file(&db, "complete-b.pkg.tar.zst", ids["complete"], Some(24)).await;
        file(&db, "partial-a.pkg.tar.zst", ids["partial"], Some(1000)).await;
        file(&db, "partial-b.pkg.tar.zst", ids["partial"], None).await;

        let listed = list_packages(&db, None, None, true).await.unwrap();
        let total = |name: &str| {
            listed
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} missing from the list"))
                .total_size
        };

        assert_eq!(total("complete"), Some(1024));
        assert_eq!(
            total("partial"),
            None,
            "a partial sum reached the column instead of being suppressed"
        );
        assert_eq!(
            total("unbuilt"),
            None,
            "a package with no artifacts should have no total, not zero"
        );
    }

    /// A row written before the size column carries `None`, which must survive
    /// to the response rather than being flattened to a zero-byte file.
    #[tokio::test]
    async fn an_unrecorded_size_stays_unknown() {
        let db = db_with_packages(&[1]).await;

        file(&db, "hello-1.0-1-x86_64.pkg.tar.zst", 1, None).await;

        let listed = package_files(&db, 1).await.unwrap();
        assert_eq!(listed[0].size, None);
    }
}

/// The status a [`ReplaceError`] answers with.
fn replace_error(e: ReplaceError) -> ApiError {
    let status = match &e {
        ReplaceError::NotFound(_) => Status::NotFound,
        ReplaceError::Refused(_) => Status::BadRequest,
        ReplaceError::Unreachable(_) => Status::BadGateway,
        ReplaceError::Internal(_) => Status::InternalServerError,
    };
    err(status, e)
}

#[utoipa::path(
    responses(
            (status = 200, description = "What could take over this dependency", body = DependencyOptions),
    ),
    params(
            ("pkgbase", description = "pkgbase of the depending package"),
            ("dependency", description = "pkgbase of the dependency to replace")
    )
)]
#[get("/package/<pkgbase>/dependency/<dependency>/options")]
pub async fn package_dependency_options(
    services: &State<Services>,
    pkgbase: &str,
    dependency: &str,
    _a: Authenticated,
) -> Result<Json<DependencyOptions>, ApiError> {
    replace::options(services, pkgbase, dependency)
        .await
        .map(Json)
        .map_err(replace_error)
}

#[utoipa::path(
    responses(
            (status = 200, description = "Point the dependency somewhere else, or drop it"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the depending package"),
            ("dependency", description = "pkgbase of the dependency being replaced")
    )
)]
#[put("/package/<pkgbase>/dependency/<dependency>", data = "<input>")]
pub async fn package_dependency_replace(
    services: &State<Services>,
    pkgbase: &str,
    dependency: &str,
    input: Json<ReplaceDependency>,
    a: Authenticated,
) -> Result<(), ApiError> {
    replace::replace(
        services,
        pkgbase,
        dependency,
        input.into_inner().replacement,
        a.username,
    )
    .await
    .map_err(replace_error)
}
