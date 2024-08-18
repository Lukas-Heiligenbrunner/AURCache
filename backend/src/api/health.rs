use bollard::Docker;
use rocket::{get, State};
use rocket_okapi::openapi;
use sea_orm::DatabaseConnection;

#[openapi(tag = "health")]
#[get("/health")]
pub async fn health(db: &State<DatabaseConnection>) -> Result<(), String> {
    check_health(db).await.map_err(|e| format!("{:?}", e))?;
    Ok(())
}

async fn check_health(db: &DatabaseConnection) -> anyhow::Result<()> {
    // check databse connection
    db.ping().await?;

    // check docker socket connection
    let docker = Docker::connect_with_unix_defaults()?;
    docker.ping().await?;

    Ok(())
}
