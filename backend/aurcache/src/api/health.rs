use bollard::Docker;
use rocket::{State, get};
use sea_orm::DatabaseConnection;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(health))]
pub struct HealthApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Internal Healthcheck")
    )
)]
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
