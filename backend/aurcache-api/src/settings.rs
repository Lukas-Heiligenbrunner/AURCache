use crate::models::authenticated::Authenticated;
use crate::models::package::PackagePatchModel;
use aurcache_types::settings::ApplicationSettings;
use aurcache_utils::settings::general::SettingsTraits;
use rocket::response::status::{BadRequest, NotFound};
use rocket::serde::json::Json;
use rocket::{State, get, patch};
use sea_orm::DatabaseConnection;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(settings, setting_update))]
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

    ApplicationSettings::get(&db, pkgid)
        .await
        .map(Json)
        .map_err(|e| NotFound(e.to_string()))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Update parts of package"),
    ),
    params(
            ("id", description = "Id of package")
    )
)]
#[patch("/setting/<pkgid>", data = "<input>")]
pub async fn setting_update(
    db: &State<DatabaseConnection>,
    input: Json<ApplicationSettings>,
    pkgid: Option<i32>,
    _a: Authenticated,
) -> Result<(), BadRequest<String>> {
    let db = db as &DatabaseConnection;

    // todo this should probably be a ApplicationSettings model with Options for each setting for being able to update just some
    ApplicationSettings::patch(&db, input.0, pkgid)
        .await
        .map(drop)
        .map_err(|e| BadRequest(e.to_string()))
}
