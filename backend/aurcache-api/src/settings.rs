use rocket::response::status::NotFound;
use rocket::serde::json::Json;
use crate::models::authenticated::Authenticated;
use rocket::{State, get};
use sea_orm::{DatabaseConnection};
use utoipa::OpenApi;
use aurcache_types::settings::ApplicationSettings;
use aurcache_utils::settings::general::{GetAllSettings};

#[derive(OpenApi)]
#[openapi(paths(settings))]
pub struct SettingsApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Get all settings", body = [ApplicationSettings]),
    )
)]
#[get("/settings?<pkgid>")]
pub async fn settings(
    db: &State<DatabaseConnection>,
    pkgid: Option<i32>,
    _a: Authenticated,
) -> Result<Json<ApplicationSettings>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    ApplicationSettings::get(&db, pkgid).await.map(Json).map_err(|e| NotFound(e.to_string()))
}