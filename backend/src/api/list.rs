use crate::aur::aur::query_aur;
use crate::db::prelude::{Builds, Packages};
use crate::db::{builds};
use crate::utils::dir_size::dir_size;
use rocket::response::status::NotFound;
use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::{get, State};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, JsonSchema};
use sea_orm::{ColumnTrait, QueryFilter};
use sea_orm::{DatabaseConnection, EntityTrait, FromQueryResult};
use sea_orm::{PaginatorTrait};

#[derive(Serialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct ApiPackage {
    name: String,
    version: String,
}

#[openapi(tag = "aur")]
#[get("/search?<query>")]
pub async fn search(query: &str) -> Result<Json<Vec<ApiPackage>>, String> {
    return match query_aur(query).await {
        Ok(v) => {
            let mapped = v
                .iter()
                .map(|x| ApiPackage {
                    name: x.name.clone(),
                    version: x.version.clone(),
                })
                .collect();
            Ok(Json(mapped))
        }
        Err(e) => Err(format!("{}", e)),
    };
}

#[derive(FromQueryResult, Deserialize, JsonSchema, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct ListStats {
    total_builds: u32,
    failed_builds: u32,
    avg_queue_wait_time: u32,
    avg_build_time: u32,
    repo_storage_size: u64,
    active_builds: u32,
    total_packages: u32,
}

#[openapi(tag = "stats")]
#[get("/stats")]
pub async fn stats(db: &State<DatabaseConnection>) -> Result<Json<ListStats>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    get_stats(db)
        .await
        .map_err(|e| NotFound(e.to_string()))
        .map(Json)
}

async fn get_stats(db: &DatabaseConnection) -> anyhow::Result<ListStats> {
    // Count total builds
    let total_builds: u32 = Builds::find().count(db).await?.try_into()?;

    // Count failed builds
    let failed_builds: u32 = Builds::find()
        .filter(builds::Column::Status.eq(2))
        .count(db)
        .await?
        .try_into()?;

    // todo implement this values somehow
    let avg_queue_wait_time: u32 = 42;
    let avg_build_time: u32 = 42;

    // Calculate repo storage size
    let repo_storage_size: u64 = dir_size("repo/").unwrap_or(0);

    // Count active builds
    let active_builds: u32 = Builds::find()
        .filter(builds::Column::Status.eq(0))
        .count(db)
        .await?
        .try_into()?;

    // Count total packages
    let total_packages: u32 = Packages::find().count(db).await?.try_into()?;

    Ok(ListStats {
        total_builds,
        failed_builds,
        avg_queue_wait_time,
        avg_build_time,
        repo_storage_size,
        active_builds,
        total_packages,
    })
}
