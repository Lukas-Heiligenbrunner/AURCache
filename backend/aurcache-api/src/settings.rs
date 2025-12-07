use rocket::response::status::{BadRequest, NotFound};
use rocket::serde::json::Json;
use crate::models::authenticated::Authenticated;
use rocket::{State, get, patch};
use sea_orm::{DatabaseConnection};
use utoipa::OpenApi;
use aurcache_types::settings::ApplicationSettings;
use aurcache_utils::settings::general::{GetAllSettings};
use crate::models::package::PackagePatchModel;

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

    ApplicationSettings::get(&db, pkgid).await.map(Json).map_err(|e| NotFound(e.to_string()))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Update parts of package"),
    ),
    params(
            ("id", description = "Id of package")
    )
)]
#[patch("/setting/<id>?<pkgid>", data = "<input>")]
pub async fn setting_update(
    db: &State<DatabaseConnection>,
    input: Json<PackagePatchModel>,
    id: i32,
    pkgid: Option<i32>,
    _a: Authenticated,
) -> Result<(), BadRequest<String>> {
    let db = db as &DatabaseConnection;
    todo!()
}