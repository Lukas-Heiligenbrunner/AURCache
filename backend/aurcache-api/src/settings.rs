use crate::models::authenticated::Authenticated;
use crate::models::settings::{SettingResponse, SettingValue};
use aurcache_types::settings::{ApplicationSettings, Setting};
use aurcache_utils::settings::general::SettingsTraits;
use rocket::http::Status;
use rocket::response::status::Custom;
use rocket::serde::json::Json;
use rocket::{State, delete, get, patch};
use sea_orm::DatabaseConnection;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(settings, setting_get, setting_patch, setting_reset))]
pub struct SettingsApi;

fn parse_setting(key: &str) -> Result<Setting, Custom<String>> {
    Setting::from_key(key)
        .ok_or_else(|| Custom(Status::NotFound, format!("Unknown setting key: {key}")))
}

#[utoipa::path(
    responses(
        (status = 200, description = "Get all settings", body = ApplicationSettings),
    ),
    params(
        ("pkgid" = Option<i32>, Query, description = "Optional package id for per-package settings"),
    )
)]
#[get("/settings?<pkgid>")]
pub async fn settings(
    db: &State<DatabaseConnection>,
    pkgid: Option<i32>,
    _a: Authenticated,
) -> Result<Json<ApplicationSettings>, Custom<String>> {
    let db = db as &DatabaseConnection;

    ApplicationSettings::get_all(db, pkgid)
        .await
        .map(Json)
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))
}

/// Fetch a single setting (any key, including the large config-file blobs).
#[utoipa::path(
    responses(
        (status = 200, description = "Get a single setting", body = SettingResponse),
        (status = 404, description = "Unknown setting key"),
    ),
    params(
        ("key" = String, Path, description = "Setting key"),
        ("pkgid" = Option<i32>, Query, description = "Optional package id"),
    )
)]
#[get("/settings/<key>?<pkgid>")]
pub async fn setting_get(
    db: &State<DatabaseConnection>,
    key: &str,
    pkgid: Option<i32>,
    _a: Authenticated,
) -> Result<Json<SettingResponse>, Custom<String>> {
    let setting = parse_setting(key)?;
    let db = db as &DatabaseConnection;

    let entry = ApplicationSettings::get::<String>(setting, pkgid, db).await;
    Ok(Json(SettingResponse {
        value: entry.value,
        env_forced: entry.env_forced,
        default: entry.default,
    }))
}

#[utoipa::path(
    responses(
        (status = 200, description = "Update a single setting"),
        (status = 404, description = "Unknown setting key"),
    ),
    params(
        ("key" = String, Path, description = "Setting key"),
        ("pkgid" = Option<i32>, Query, description = "Optional package id"),
    )
)]
#[patch("/settings/<key>?<pkgid>", data = "<input>")]
pub async fn setting_patch(
    db: &State<DatabaseConnection>,
    key: &str,
    pkgid: Option<i32>,
    input: Json<SettingValue>,
    _a: Authenticated,
) -> Result<(), Custom<String>> {
    let setting = parse_setting(key)?;
    let db = db as &DatabaseConnection;

    ApplicationSettings::patch(db, [(setting, pkgid, Some(input.value.clone()))])
        .await
        .map_err(|e| Custom(Status::BadRequest, e.to_string()))
}

/// Reset a setting back to its default by deleting any stored override.
#[utoipa::path(
    responses(
        (status = 200, description = "Reset a setting to its default"),
        (status = 404, description = "Unknown setting key"),
    ),
    params(
        ("key" = String, Path, description = "Setting key"),
        ("pkgid" = Option<i32>, Query, description = "Optional package id"),
    )
)]
#[delete("/settings/<key>?<pkgid>")]
pub async fn setting_reset(
    db: &State<DatabaseConnection>,
    key: &str,
    pkgid: Option<i32>,
    _a: Authenticated,
) -> Result<(), Custom<String>> {
    let setting = parse_setting(key)?;
    let db = db as &DatabaseConnection;

    ApplicationSettings::patch(db, [(setting, pkgid, None)])
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))
}
