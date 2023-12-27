use crate::repo::repo::{remove_pkg, remove_version};
use rocket::serde::json::Json;
use rocket::serde::Deserialize;
use rocket::{post, State};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, JsonSchema};
use sea_orm::DatabaseConnection;

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct DelBody {
    id: i32,
}

#[openapi(tag = "test")]
#[post("/packages/delete", data = "<input>")]
pub async fn package_del(
    db: &State<DatabaseConnection>,
    input: Json<DelBody>,
) -> Result<(), String> {
    let db = db as &DatabaseConnection;
    let pkg_id = input.id.clone();

    remove_pkg(db, pkg_id).await.map_err(|e| e.to_string())?;

    Ok(())
}

#[openapi(tag = "test")]
#[post("/versions/delete/<id>")]
pub async fn version_del(db: &State<DatabaseConnection>, id: i32) -> Result<(), String> {
    let db = db as &DatabaseConnection;

    remove_version(db, id).await.map_err(|e| e.to_string())?;

    Ok(())
}
