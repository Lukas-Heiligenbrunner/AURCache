use crate::db::builds;
use crate::db::prelude::{Builds, Packages};
use crate::utils::dir_size::dir_size;

use rocket::response::status::NotFound;
use rocket::serde::json::Json;

use rocket::{get, State};

use crate::api::types::input::ListStats;
use rocket_okapi::openapi;
use sea_orm::prelude::Expr;
use sea_orm::sea_query::Function::Avg;
use sea_orm::sea_query::{Func, QueryStatement};
use sea_orm::{ColumnTrait, QueryFilter};
use sea_orm::{DatabaseConnection, EntityTrait};
use sea_orm::{DbBackend, FromQueryResult, PaginatorTrait, QuerySelect, Statement, TryGetableMany};

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

    // Calculate repo storage size
    let repo_storage_size: u64 = dir_size("repo/").unwrap_or(0);

    // Count active builds
    let enqueued_builds: u32 = Builds::find()
        .filter(builds::Column::Status.eq(3))
        .count(db)
        .await?
        .try_into()?;

    #[derive(Debug, FromQueryResult)]
    struct BuildTimeStruct {
        avg_build_time: Option<f64>,
    }

    let unique: BuildTimeStruct =
        BuildTimeStruct::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT AVG((builds.end_time - builds.start_time)) AS avg_build_time
        FROM builds
        WHERE builds.end_time IS NOT NULL AND builds.status = 1;"#,
            [],
        ))
        .one(db)
        .await?
        .ok_or(anyhow::anyhow!("No Average build time"))?;

    let avg_build_time: u32 = unique.avg_build_time.unwrap_or(0.0) as u32;

    // Count total packages
    let total_packages: u32 = Packages::find().count(db).await?.try_into()?;

    Ok(ListStats {
        total_builds,
        failed_builds,
        avg_queue_wait_time,
        avg_build_time,
        repo_storage_size,
        enqueued_builds,
        total_packages,
    })
}
