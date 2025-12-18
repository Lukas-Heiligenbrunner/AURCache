use crate::models::authenticated::Authenticated;
use aurcache_types::settings::ApplicationSettings;
use aurcache_utils::settings::general::SettingsTraits;
use rocket::response::status::{BadRequest, NotFound};
use rocket::serde::json::Json;
use rocket::{State, get, patch};
use sea_orm::DatabaseConnection;
use utoipa::OpenApi;
use aurcache_utils::settings::types::SettingType;
use crate::models::settings::ApplicationSettingsPatch;

#[derive(OpenApi)]
#[openapi(paths(settings, setting_update_package, setting_update))]
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
            (status = 200, description = "Update settings for specific package id"),
    ),
    params(
            ("pkgid", description = "Id of package")
    )
)]
#[patch("/setting/<pkgid>", data = "<input>")]
pub async fn setting_update_package(
    db: &State<DatabaseConnection>,
    input: Json<ApplicationSettingsPatch>,
    pkgid: i32,
    _a: Authenticated,
) -> Result<(), BadRequest<String>> {
    let db = db as &DatabaseConnection;

    let mut changedsettings = vec![];

    // cpu limit
    if let Some(cpu_limt) = input.cpu_limit {
        changedsettings.push((SettingType::CpuLimit,
                              Some(pkgid),
                              cpu_limt.map(|v| v.to_string())))
    }

    // memory limit
    if let Some(memory_limit) = input.memory_limit {
        changedsettings.push((SettingType::MemoryLimit,
                              None,
                              memory_limit.map(|v| v.to_string())))
    }

    // todo change this ifletsome into helper functions

    ApplicationSettings::patch(&db, changedsettings)
        .await
        .map(drop)
        .map_err(|e| BadRequest(e.to_string()))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Update global settings"),
    ),
    params(

    )
)]
#[patch("/setting", data = "<input>")]
pub async fn setting_update(
    db: &State<DatabaseConnection>,
    input: Json<ApplicationSettingsPatch>,
    _a: Authenticated,
) -> Result<(), BadRequest<String>> {
    let db = db as &DatabaseConnection;

    let mut changedsettings = vec![];

    // cpu limit
    if let Some(cpu_limt) = input.cpu_limit {
        changedsettings.push((SettingType::CpuLimit,
                              None,
                              cpu_limt.map(|v| v.to_string())))
    }

    // memory limit
    if let Some(memory_limit) = input.memory_limit {
        changedsettings.push((SettingType::MemoryLimit,
                              None,
                              memory_limit.map(|v| v.to_string())))
    }

    // todo change this ifletsome into helper functions

    ApplicationSettings::patch(&db, changedsettings)
        .await
        .map(drop)
        .map_err(|e| BadRequest(e.to_string()))
}
