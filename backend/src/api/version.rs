use crate::repo::repo::remove_version;
use rocket::{delete, State};
use rocket_okapi::openapi;
use sea_orm::DatabaseConnection;

#[openapi(tag = "version")]
#[delete("/version/<id>/delete")]
pub async fn version_del(db: &State<DatabaseConnection>, id: i32) -> Result<(), String> {
    let db = db as &DatabaseConnection;

    remove_version(db, id).await.map_err(|e| e.to_string())?;

    Ok(())
}
