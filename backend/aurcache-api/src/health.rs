use aurcache_common::api::info::{ServerInfo, Timezone};
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{State, get};
use sea_orm::DatabaseConnection;
use utoipa::OpenApi;

use crate::init::ServerVersion;

#[derive(OpenApi)]
#[openapi(
    paths(health, server_version),
    components(schemas(ServerInfo, Timezone))
)]
pub struct HealthApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Internal Healthcheck"),
            (status = 500, description = "Database unreachable"),
    )
)]
#[get("/health")]
pub async fn health(db: &State<DatabaseConnection>) -> Result<(), Status> {
    // `{:#}` rather than `{}`: the outer message alone is usually just
    // "connection error", and the cause underneath it is the part worth
    // reading. It goes to the logs, not the wire: a health poller only needs
    // the status code, and the success case must stay an empty 200 (the
    // client distinguishes the API from the web UI by its empty body).
    if let Err(e) = check_health(db).await {
        tracing::error!("health check failed: {e:#}");
        return Err(Status::InternalServerError);
    }
    Ok(())
}

/// The running server's own version.
///
/// The bundled UI shows this rather than its own crate version, so the two
/// agree about what is running. Separate from [`health`] because that route's
/// empty 200 is load-bearing (see above), and unauthenticated like it: a
/// version string says nothing worth protecting, and the startup log says it
/// anyway.
#[utoipa::path(
    responses(
            (status = 200, description = "The running server's version", body = ServerInfo),
    )
)]
#[get("/version")]
pub fn server_version(version: &State<ServerVersion>) -> Json<ServerInfo> {
    Json(ServerInfo {
        version: version.0.clone(),
        timezone: Some(local_timezone()),
    })
}

/// The zone the server's clock -- and so the scheduler -- is using.
///
/// `TZ` first, spelled as it was set. Otherwise the name comes from where
/// `/etc/localtime` links; the images
/// ship without that link so that a host's zone bind-mounted over it is not
/// mislabelled as the image's `Etc/UTC`, and is reported by offset alone.
fn local_timezone() -> Timezone {
    let name = match std::env::var("TZ") {
        // An empty `TZ` is UTC to `Local`, not "unset".
        Ok(tz) if tz.is_empty() => Some("UTC".to_string()),
        Ok(tz) => Some(tz.trim_start_matches(':').to_string()),
        Err(_) => jiff::tz::TimeZone::system().iana_name().map(str::to_string),
    };
    Timezone {
        name,
        utc_offset: jiff::Zoned::now().offset().seconds(),
    }
}

async fn check_health(db: &DatabaseConnection) -> anyhow::Result<()> {
    // Check database connection.
    db.ping().await?;

    Ok(())
}
