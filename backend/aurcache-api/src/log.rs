use crate::models::authenticated::Authenticated;
use crate::utils::error::{ApiError, err};
use crate::utils::pagination::clamp_limit;
use aurcache_activitylog::activity_utils::Severity;
use aurcache_activitylog::log_store::{LogFilter, LogStore};
use aurcache_common::api::log::{EntityRef, LogPage};
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{State, get};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(log))]
pub struct LogApi;

/// One page of the log, newest first, with how long the filtered log is.
///
/// Paged and filtered here rather than in the browser: the log only grows, so
/// there is no point at which fetching all of it is the cheap option.
///
/// `entity` is a reference such as `pkg:hello`, `worker:builder-01` or
/// `build:hello/7`, and finds every entry naming it in any role -- narrowed to
/// one by `role`. A build also finds what was recorded during it.
#[utoipa::path(
    params(
        ("limit" = Option<u64>, Query, description = "Page size"),
        ("offset" = Option<u64>, Query, description = "Entries to skip"),
        ("severity" = Option<String>, Query, description = "`info`, `warning` or `error`: that level and worse"),
        ("since_boot" = Option<bool>, Query, description = "Only what happened since the server last started"),
        ("kind" = Option<String>, Query, description = "Only entries of this kind"),
        ("entity" = Option<String>, Query, description = "Only entries naming this entity, e.g. `pkg:hello`"),
        ("role" = Option<String>, Query, description = "With `entity`: only where it played this role"),
    ),
    responses(
        (status = 200, description = "One page of the log, newest first", body = LogPage),
        (status = 400, description = "`entity` is not a reference"),
    )
)]
#[get("/log?<limit>&<offset>&<severity>&<since_boot>&<kind>&<entity>&<role>")]
#[allow(clippy::too_many_arguments)]
pub async fn log(
    _a: Authenticated,
    db: &State<sea_orm::DatabaseConnection>,
    limit: Option<u64>,
    offset: Option<u64>,
    severity: Option<String>,
    since_boot: Option<bool>,
    kind: Option<String>,
    entity: Option<String>,
    role: Option<String>,
) -> Result<Json<LogPage>, ApiError> {
    let limit = clamp_limit(limit);
    // A malformed entity is refused rather than ignored: ignoring it would
    // answer "everything" to a question about one thing.
    let entity = entity
        .as_deref()
        .map(str::parse::<EntityRef>)
        .transpose()
        .map_err(|e| err(Status::BadRequest, e))?;
    // A severity nobody recognises narrows nothing, the way an unreadable
    // filter in a URL degrades to "any" everywhere else in this app.
    let filter = LogFilter {
        severity: severity.as_deref().and_then(Severity::from_slug),
        since_boot: since_boot.unwrap_or(false),
        kind,
        entity,
        role,
    };
    let page = LogStore::new(db.inner().clone())
        .page(limit, offset.unwrap_or(0), &filter)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(Json(page))
}
