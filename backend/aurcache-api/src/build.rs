use aurcache_utils::services::Services;
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{Request, State, delete, get, post};

use crate::auth::Authenticated;
use crate::utils::error::{ApiError, err};
use crate::utils::pagination::page_offset;
use aurcache_activitylog::events::{Event, QueueCause};
use aurcache_common::api::builds::BuildSummary;
use aurcache_common::api::log::BuildRef;
use aurcache_common::build_state::{BuildState, BuildTrigger};
use aurcache_db::prelude::Builds;
use aurcache_db::{builds, packages};
use aurcache_utils::build_delete::Delete;
use aurcache_utils::build_logger::{build_log_path, build_log_size, read_build_output};
use aurcache_utils::cancel::Cancel;
use aurcache_utils::listing::{BuildRow, annotate_waiting, build_row_select, waiting_reasons};
use aurcache_utils::package::update::{Rebuild, package_update, queued};
use rocket::fs::NamedFile;
use rocket::http::{ContentType, Header};
use rocket::response::Responder;
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, JoinType, Order, QueryFilter, QueryOrder,
    QuerySelect, RelationTrait, Select,
};
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
        .await?
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
    let pkg = Some(
        crate::package::package_by_pkgbase(db.inner(), pkgbase)
            .await?
            .id,
    );
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
    // Every row has a start time, so the bare column orders identically on
    // both backends and the `start_time` index serves it.
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
    let rows = query.into_model::<BuildRow>().all(db).await?;

    Ok(Json(annotate_waiting(db, rows).await))
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
    .await?
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
        .await?
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
    services: &State<Services>,
    pkgbase: &str,
    number: i32,
    a: Authenticated,
) -> Result<(), ApiError> {
    let db = &services.db;

    let build = build_by_number(db, pkgbase, number).await?;
    match aurcache_utils::build_delete::delete_build(db, pkgbase, &build).await? {
        Delete::Deleted => {}
        Delete::Running => {
            return Err(err(
                Status::Conflict,
                format!("build {pkgbase}/{number} is running; cancel it first"),
            ));
        }
    }
    services.activity.emit_by(
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
    services: &State<Services>,
    pkgbase: &str,
    number: i32,
    a: Authenticated,
) -> Result<(), ApiError> {
    let db = &services.db;
    let build = build_by_number(db, pkgbase, number).await?;
    match aurcache_utils::cancel::cancel_build(db, pkgbase, &build).await? {
        Cancel::Cancelled => {}
        Cancel::Settled => {
            return Err(err(
                Status::Conflict,
                format!("build {pkgbase}/{number} has already finished"),
            ));
        }
    }
    services.activity.emit_by(
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
) -> Result<Json<i32>, ApiError> {
    let db = &services.db;

    // The build being retried tells us which platform and package to rebuild.
    let old_build = build_by_number(db, pkgbase, number).await?;
    let platform = old_build.platform;
    let package = crate::package::package_by_id(db, old_build.pkg_id).await?;

    // Route retries through the same path as "Force Rebuild": this re-fetches
    // the .SRCINFO, resolves AUR dependencies again, and syncs the dependency
    // graph before enqueuing builds, instead of blindly re-enqueuing the old
    // build's stored version with a stale dependency graph.
    // An operator's retry: recorded as theirs on the build rows, and in the
    // log against the build they retried, so a build nobody remembers queueing
    // can be traced to whoever did.
    let platform_results = package_update(services, package, Rebuild::Always, BuildTrigger::User)
        .await?
        // A retry always builds, so it is never up to date.
        .builds();
    // "Retry" on a build that worked asks for it again: a rebuild.
    services.activity.emit_by(
        queued(
            pkgbase,
            QueueCause::after(old_build.status == BuildState::Failed),
            &platform_results,
        )
        .retrying(BuildRef {
            pkgbase: pkgbase.to_string(),
            number,
        }),
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
