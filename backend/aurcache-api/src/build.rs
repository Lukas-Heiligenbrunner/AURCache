use aurcache_utils::services::Services;
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{Request, State, delete, get, post};

use crate::models::authenticated::Authenticated;
use crate::models::builds::BuildSummary;
use crate::utils::error::{ApiError, err};
use crate::utils::pagination::page_offset;
use crate::worker::liveness_timeout_secs;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, QueueCause};
use aurcache_common::api::builds::{DiskUsage, KeptBuild};
use aurcache_common::api::log::BuildRef;
use aurcache_common::api::waiting::WaitingReason;
use aurcache_common::build_state::{BuildState, BuildTrigger};
use aurcache_db::helpers::worker_jobs;
use aurcache_db::prelude::Builds;
use aurcache_db::{builds, packages, workers};
use aurcache_utils::build_logger::{
    build_log_path, build_log_size, read_build_output, remove_build_log,
};
use aurcache_utils::cancel::Cancel;
use aurcache_utils::package::update::{package_update, queued};
use rocket::fs::NamedFile;
use rocket::http::{ContentType, Header};
use rocket::response::Responder;
use sea_orm::FromQueryResult;
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, JoinType, Order, QueryFilter, QueryOrder,
    QuerySelect, RelationTrait, Select,
};
use std::collections::HashMap;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(
    build_output,
    download_build_output,
    list_builds,
    list_package_builds,
    get_build,
    delete_build,
    cancel_build,
    retry_build
))]
pub struct BuildApi;

/// Slice a build's stored output for the caller.
///
/// `offset` is how many bytes of log the caller already holds, so the response
/// carries only what is new; an out-of-range offset simply skips nothing or
/// everything rather than erroring. `limit` bounds the response, defaulting to
/// and clamped to `MAX_OUTPUT_BYTES` server-side, so one request's size never
/// scales with the log.
///
/// The body is raw file bytes, not decoded text: a slice may end mid-character,
/// and re-aligning on the next offset is the caller's job.
///
/// A build that exists but has not written anything yet yields an empty body.
/// "No output yet" is the normal state of a freshly started build, and the log
/// view polls this endpoint from the moment it opens — reporting that as an
/// error would make every new build's first poll fail.
#[utoipa::path(
    responses(
            (status = 200, description = "Up to `limit` bytes of build output from `offset` onwards, raw; empty if the build has not logged anything yet"),
            (status = 404, description = "No such build"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("number", description = "Build number within that package"),
            ("offset", description = "Bytes of log the caller already has; the response starts there"),
            ("limit", description = "Maximum bytes to return; defaults to and is clamped to the server bound")
    )
)]
#[get("/package/<pkgbase>/build/<number>/output?<offset>&<limit>")]
pub async fn build_output(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    number: i32,
    offset: Option<u64>,
    limit: Option<u64>,
    _a: Authenticated,
) -> Result<(ContentType, Vec<u8>), ApiError> {
    let db = db.inner();

    // Resolved even though the log path needs neither: an unknown
    // pkgbase/number must still be a 404 rather than an empty log, which would
    // be indistinguishable from a build that produced no output.
    build_by_number(db, pkgbase, number).await?;

    // A build with no log file is not an error: it may have produced nothing
    // yet, or its log may have been removed. Callers render the difference.
    let bytes = read_build_output(pkgbase, number, offset.unwrap_or(0), limit)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .unwrap_or_default();

    Ok((ContentType::Plain, bytes))
}

/// A [`NamedFile`] served as a `Content-Disposition: attachment` download.
///
/// `NamedFile`'s responder streams the file in chunks, which is why this is a
/// wrapper rather than a `Vec<u8>`: a large log is never buffered whole just
/// because someone asked for the full log. (`NamedFile` alone would set the
/// Content-Type from the `.log` extension and stream too, but there is no way
/// to attach the download header through it.)
pub struct LogDownload(NamedFile, String);

impl<'r> Responder<'r, 'static> for LogDownload {
    fn respond_to(self, req: &'r Request<'_>) -> rocket::response::Result<'static> {
        let mut response = self.0.respond_to(req)?;
        response.set_header(Header::new("Content-Disposition", self.1));
        Ok(response)
    }
}

/// Download a build's whole log as an attachment.
///
/// The entire file, with none of the caps that bound [`build_output`]: this is
/// the escape hatch for the rare occasions the full log is genuinely wanted.
/// The log view stays within its window and copies from that; it links here for
/// "give me the whole thing". The file is streamed from disk, so the response
/// does not hold the log in memory, and the attachment name is the build's
/// public identity, `<pkgbase>-<number>.log`.
///
/// A build whose log file does not exist -- never wrote output, or the log was
/// removed -- is a 404 here, deliberately different from [`build_output`],
/// where an empty body means "nothing new yet". Downloading a log that is not
/// there is a mistake, not a poll.
#[utoipa::path(
    responses(
            (status = 200, description = "The build's log, as an attachment named `<pkgbase>-<number>.log`"),
            (status = 404, description = "No such build, or no log for it"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("number", description = "Build number within that package")
    )
)]
#[get("/package/<pkgbase>/build/<number>/output/download")]
pub async fn download_build_output(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    number: i32,
    _a: Authenticated,
) -> Result<LogDownload, ApiError> {
    // Resolved even though the open could fail on the log path: an unknown
    // pkgbase/number must be a 404 with the build's name, not a generic error.
    build_by_number(db.inner(), pkgbase, number).await?;

    let file = NamedFile::open(build_log_path(pkgbase, number))
        .await
        .map_err(|e| {
            err(
                Status::NotFound,
                format!("no log for build {pkgbase}/{number}: {e}"),
            )
        })?;
    Ok(LogDownload(file, content_disposition(pkgbase, number)))
}

#[utoipa::path(
    responses(
            (status = 200, description = "List of all builds"),
    ),
    params(
            ("limit", description = "Limit of items to fetch"),
            ("page", description = "Page to fetch"),
            ("worker", description = "Only builds this worker (by id) ran or is running"),
            ("status", description = "Only builds in these states, comma-separated keys: \
                active, successful, failed, enqueued, waiting-for-deps, publishing"),
    )
)]
/// Builds across all packages, newest first.
#[get("/builds?<limit>&<page>&<worker>&<status>")]
pub async fn list_builds(
    db: &State<DatabaseConnection>,
    limit: Option<u64>,
    page: Option<u64>,
    worker: Option<i32>,
    status: Option<&str>,
    _a: Authenticated,
) -> Result<Json<Vec<BuildSummary>>, ApiError> {
    let filter = BuildFilter::new(None, worker, status)?;
    list_builds_impl(db.inner(), &filter, limit, page).await
}

/// Builds for one package.
///
/// A sub-resource of the package rather than a `?pkgbase=` filter: a pkgbase
/// may contain `+`, which decodes to a space in a query value but is literal in
/// a path segment.
#[utoipa::path(
    responses((status = 200, description = "List builds for a package", body = Vec<BuildSummary>)),
    params(
        ("pkgbase" = String, Path, description = "pkgbase of the package"),
        ("limit", description = "Limit of items to fetch"),
        ("page", description = "Page to fetch"),
        ("worker", description = "Only builds this worker (by id) ran or is running"),
        ("status", description = "Only builds in these states, comma-separated keys"),
    )
)]
#[get("/package/<pkgbase>/builds?<limit>&<page>&<worker>&<status>")]
pub async fn list_package_builds(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    limit: Option<u64>,
    page: Option<u64>,
    worker: Option<i32>,
    status: Option<&str>,
    _a: Authenticated,
) -> Result<Json<Vec<BuildSummary>>, ApiError> {
    let pkg = Some(crate::package::package_id(db.inner(), pkgbase).await?);
    let filter = BuildFilter::new(pkg, worker, status)?;
    list_builds_impl(db.inner(), &filter, limit, page).await
}

/// Which builds a list is narrowed to.
struct BuildFilter {
    pkg_id: Option<i32>,
    /// The worker that ran the build, or is running it.
    worker: Option<i32>,
    /// Empty means every state.
    states: Vec<BuildState>,
}

impl BuildFilter {
    fn new(
        pkg_id: Option<i32>,
        worker: Option<i32>,
        status: Option<&str>,
    ) -> Result<Self, ApiError> {
        let states = status
            .map(BuildState::parse_keys)
            .transpose()
            .map_err(|e| err(Status::BadRequest, e))?
            .unwrap_or_default();
        Ok(Self {
            pkg_id,
            worker,
            states,
        })
    }
}

async fn list_builds_impl(
    db: &DatabaseConnection,
    filter: &BuildFilter,
    limit: Option<u64>,
    page: Option<u64>,
) -> Result<Json<Vec<BuildSummary>>, ApiError> {
    // Every row has a start time since the dashboard-indexes migration
    // backfilled legacy NULLs, so the bare column orders identically on both
    // backends and the `start_time` index serves it.
    let mut query = build_row_select()
        .order_by(builds::Column::StartTime, Order::Desc)
        .limit(limit)
        .offset(page_offset(page, limit));
    if let Some(pkg_id) = filter.pkg_id {
        query = query.filter(builds::Column::PkgId.eq(pkg_id));
    }
    if let Some(worker) = filter.worker {
        query = query.filter(builds::Column::WorkerId.eq(worker));
    }
    if !filter.states.is_empty() {
        query = query.filter(builds::Column::Status.is_in(filter.states.iter().copied()));
    }
    let rows = query
        .into_model::<BuildRow>()
        .all(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    Ok(Json(annotate_waiting(db, rows).await))
}

/// The build-list projection shared by the list, single-build and dashboard routes.
///
/// Every listed build has the same fields, so there is one place the shape of a
/// [`BuildRow`] is described.
pub(crate) fn build_row_select() -> Select<Builds> {
    Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .select_only()
        .column_as(builds::Column::Id, "id")
        .column_as(builds::Column::Number, "number")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column(builds::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .column(builds::Column::Platform)
        .column(builds::Column::Size)
        .column(builds::Column::PeakMemory)
        .column(builds::Column::DiskChroot)
        .column(builds::Column::DiskWorkdir)
        .column(builds::Column::DiskSources)
        .column(builds::Column::DiskBuildTree)
        .column(builds::Column::KeptPath)
        .column(builds::Column::KeptUntil)
        .column(builds::Column::KeptTree)
        // Left, so a queued build -- which has no worker yet -- still lists.
        .join(JoinType::LeftJoin, builds::Relation::Workers.def())
        .column_as(workers::Column::Name, "worker_name")
}

/// A listed build as queried, including the row id the response omits.
///
/// Waiting reasons are keyed by row id internally, so the id has to survive as
/// far as the annotation — but no further.
#[derive(FromQueryResult)]
pub(crate) struct BuildRow {
    id: i32,
    number: i32,
    pkg_name: String,
    version: String,
    status: BuildState,
    start_time: Option<i64>,
    end_time: Option<i64>,
    platform: String,
    size: Option<i64>,
    peak_memory: Option<i64>,
    disk_chroot: Option<i64>,
    disk_workdir: Option<i64>,
    disk_sources: Option<i64>,
    disk_build_tree: Option<i64>,
    kept_path: Option<String>,
    kept_until: Option<i64>,
    kept_tree: Option<String>,
    worker_name: Option<String>,
}

impl BuildRow {
    fn into_summary(self, waiting_reason: Option<WaitingReason>) -> BuildSummary {
        BuildSummary {
            number: self.number,
            pkg_name: self.pkg_name,
            version: self.version,
            status: self.status,
            start_time: self.start_time,
            end_time: self.end_time,
            platform: self.platform,
            size: self.size,
            peak_memory: self.peak_memory,
            disk_usage: Some(DiskUsage {
                chroot: self.disk_chroot,
                workdir: self.disk_workdir,
                sources: self.disk_sources,
                build_tree: self.disk_build_tree,
            })
            .filter(|usage| !usage.is_empty()),
            kept: self
                .kept_path
                .zip(self.kept_until)
                .map(|(path, until)| KeptBuild {
                    path,
                    until,
                    tree: self.kept_tree,
                }),
            worker_name: self.worker_name,
            // Filled only by the detail route, which knows it is rendering one
            // build; the lists would pay a stat per row for fields they do not
            // show. See the field's docs for how `None` differs from `0`.
            log_size: None,
            waiting_reason,
        }
    }
}

/// Attach [`WaitingReason`]s to any listed build that no approved worker can
/// currently claim, so a stalled build is distinguishable from a queued one.
///
/// Best-effort by design: this is diagnostic decoration, and failing to compute
/// it must never turn a working build list into an error page.
pub(crate) async fn annotate_waiting(
    db: &DatabaseConnection,
    rows: Vec<BuildRow>,
) -> Vec<BuildSummary> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut reasons = waiting_reasons(db).await;
    rows.into_iter()
        .map(|row| {
            let reason = reasons.remove(&row.id);
            row.into_summary(reason)
        })
        .collect()
}

/// Why each queued build is waiting, by row id; empty when that cannot be
/// computed.
async fn waiting_reasons(db: &DatabaseConnection) -> HashMap<i32, WaitingReason> {
    worker_jobs::waiting_reasons(db, liveness_timeout_secs())
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("could not compute build waiting reasons: {e}");
            HashMap::new()
        })
}

/// Scope a build select to one public build identity — `<pkgbase>/<number>`.
///
/// The row id never leaves the server: it is a global sequence that says
/// nothing about which package a build belongs to. Everything public keys on
/// the package and the build's number within it, so that rule lives here and a
/// change to it (e.g. scoping) lands once for both resolvers below.
fn build_identity(select: Select<Builds>, pkgbase: &str, number: i32) -> Select<Builds> {
    select
        .filter(packages::Column::Name.eq(pkgbase))
        .filter(builds::Column::Number.eq(number))
}

/// `Content-Disposition` for a build-log download.
///
/// The pkgbase is interpolated into a response header, so it is confined to
/// the package-name alphabet first: a crafted name reaching this line would
/// otherwise turn into response-header injection. Legitimate names pass
/// through untouched.
fn content_disposition(pkgbase: &str, number: i32) -> String {
    let safe: String = pkgbase
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '+' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("attachment; filename=\"{safe}-{number}.log\"")
}

/// Resolve a public build identity — `<pkgbase>/<number>` — to its row.
async fn build_by_number(
    db: &DatabaseConnection,
    pkgbase: &str,
    number: i32,
) -> Result<builds::Model, ApiError> {
    build_identity(
        Builds::find().join_rev(JoinType::InnerJoin, packages::Relation::Builds.def()),
        pkgbase,
        number,
    )
    .one(db)
    .await
    .map_err(|e| err(Status::InternalServerError, e))?
    .ok_or_else(|| err(Status::NotFound, format!("no build {pkgbase}/{number}")))
}

/// Resolve a single build to its [`BuildRow`] projection, for the detail route.
async fn build_row_by_number(
    db: &DatabaseConnection,
    pkgbase: &str,
    number: i32,
) -> Result<BuildRow, ApiError> {
    build_identity(build_row_select(), pkgbase, number)
        .into_model::<BuildRow>()
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, format!("no build {pkgbase}/{number}")))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get build details"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("number", description = "Build number within that package")
    )
)]
#[get("/package/<pkgbase>/build/<number>")]
pub async fn get_build(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    number: i32,
    _a: Authenticated,
) -> Result<Json<BuildSummary>, ApiError> {
    let db = db.inner();

    let row = build_row_by_number(db, pkgbase, number).await?;
    // Waiting reasons only exist for queued builds, and computing them scans
    // the whole queue plus the fleet — skip that for a build whose status
    // already says the annotation would be `None`.
    let reason = if row.status == BuildState::Enqueued {
        waiting_reasons(db).await.remove(&row.id)
    } else {
        None
    };
    let mut summary = row.into_summary(reason);
    // The one field only the detail route fills: a metadata stat, not a read.
    summary.log_size = build_log_size(pkgbase, number)
        .await
        .and_then(|s| i64::try_from(s).ok());
    Ok(Json(summary))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Delete build"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("number", description = "Build number within that package")
    )
)]
#[delete("/package/<pkgbase>/build/<number>")]
pub async fn delete_build(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    number: i32,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<(), ApiError> {
    let db = db.inner();

    let build = build_by_number(db, pkgbase, number).await?;
    // A worker holds an active build's lease and a publishing one is being
    // moved into the repository; removing the row under either leaves that
    // work pointing at nothing.
    if matches!(build.status, BuildState::Active | BuildState::Publishing) {
        return Err(err(
            Status::Conflict,
            format!("build {pkgbase}/{number} is running; cancel it first"),
        ));
    }

    aurcache_db::helpers::builds::delete_build(db, &build)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    remove_build_log(pkgbase, number).await;
    al.emit_by(
        Event::BuildDeleted {
            build: BuildRef {
                pkgbase: pkgbase.to_string(),
                number,
            },
        },
        a.username,
    );

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Cancelled"),
            (status = 404, description = "No such build"),
            (status = 409, description = "The build has already finished, or is being published"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("number", description = "Build number within that package")
    )
)]
#[post("/package/<pkgbase>/build/<number>/cancel")]
pub async fn cancel_build(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    number: i32,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<(), ApiError> {
    let build = build_by_number(db.inner(), pkgbase, number).await?;
    match aurcache_utils::cancel::cancel_build(db.inner(), pkgbase, &build)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
    {
        Cancel::Cancelled => {}
        Cancel::Settled => {
            return Err(err(
                Status::Conflict,
                format!("build {pkgbase}/{number} has already finished"),
            ));
        }
    }
    al.emit_by(
        Event::BuildCancelled {
            build: BuildRef {
                pkgbase: pkgbase.to_string(),
                number,
            },
        },
        a.username,
    );

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Retry build"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("number", description = "Build number within that package"),
    )
)]
#[post("/package/<pkgbase>/build/<number>/retry")]
pub async fn retry_build(
    services: &State<Services>,
    pkgbase: &str,
    number: i32,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<Json<i32>, ApiError> {
    let db = &services.db;

    // The build being retried tells us which platform and package to rebuild.
    let old_build = build_by_number(db, pkgbase, number).await?;
    let platform = old_build.platform;
    let pkg_id = old_build.pkg_id;

    // Fetch the package details
    let package = packages::Entity::find_by_id(pkg_id)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "no package with that id"))?;

    // Route retries through the same path as "Force Rebuild": this re-fetches
    // the .SRCINFO, resolves AUR dependencies again, and syncs the dependency
    // graph before enqueuing builds, instead of blindly re-enqueuing the old
    // build's stored version with a stale dependency graph.
    // An operator's retry: recorded as theirs on the build rows, and in the
    // log against the build they retried, so a build nobody remembers queueing
    // can be traced to whoever did.
    let platform_results = package_update(services, package, true, BuildTrigger::User)
        .await
        // As for an update: an unresolvable source or a patch that no longer
        // applies is the caller's to fix.
        .map_err(|e| err(Status::BadRequest, e))?;
    // "Retry" on a build that worked asks for it again: a rebuild.
    al.emit_by(
        queued(
            pkgbase,
            QueueCause::after(old_build.status == BuildState::Failed),
            &platform_results,
            Some(BuildRef {
                pkgbase: pkgbase.to_string(),
                number,
            }),
            None,
        ),
        a.username,
    );

    // Pick out the build explicitly reported for the platform that was
    // retried; it may have been enqueued/promoted or left waiting on a
    // dependency rebuild, either way `package_update` already tells us its id.
    let build_number = platform_results
        .into_iter()
        .find(|r| r.platform == platform)
        .map(|r| r.build_number)
        .ok_or_else(|| {
            err(
                Status::InternalServerError,
                "no build was enqueued for retry",
            )
        })?;

    Ok(Json(build_number))
}
