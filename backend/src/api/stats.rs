use crate::db::builds;
use crate::db::prelude::{Builds, Packages};
use crate::utils::dir_size::dir_size;
use bigdecimal::ToPrimitive;

use rocket::response::status::NotFound;
use rocket::serde::json::Json;

use crate::api::types::authenticated::Authenticated;
use crate::api::types::input::{GraphDataPoint, ListStats, UserInfo};
use crate::builder::types::BuildStates;
use crate::db::helpers::dbtype::{database_type, DbType};
use rocket::{get, State};
use sea_orm::prelude::BigDecimal;
use sea_orm::{ColumnTrait, QueryFilter};
use sea_orm::{DatabaseConnection, EntityTrait};
use sea_orm::{DbBackend, FromQueryResult, PaginatorTrait, Statement};
use utoipa::{OpenApi};

#[derive(OpenApi)]
#[openapi(paths(stats, dashboard_graph_data, user_info))]
pub struct StatsApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Get general build-server stats", body = [ListStats]),
    )
)]
#[get("/stats")]
pub async fn stats(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<ListStats>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    get_stats(db)
        .await
        .map_err(|e| NotFound(e.to_string()))
        .map(Json)
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get infos about the signed in user", body = [UserInfo]),
    )
)]
#[get("/userinfo")]
pub async fn user_info(a: Authenticated) -> Json<UserInfo> {
    Json(UserInfo {
        username: a.username,
    })
}

#[utoipa::path(
    responses(
        (status = 200, description = "Get graph data for dashboard", body = [Vec<GraphDataPoint>]),
    )
)]
#[get("/graph")]
pub async fn dashboard_graph_data(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<Vec<GraphDataPoint>>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    get_graph_datapoints(db)
        .await
        .map_err(|e| NotFound(e.to_string()))
        .map(Json)
}

async fn get_graph_datapoints(db: &DatabaseConnection) -> anyhow::Result<Vec<GraphDataPoint>> {
    let query = match database_type() {
        DbType::Sqlite => {
            "SELECT
    CAST(strftime('%Y', datetime(start_time, 'unixepoch')) AS INTEGER) AS year,
    CAST(strftime('%m', datetime(start_time, 'unixepoch')) AS INTEGER) AS month,
    COUNT(*) AS count
FROM
    builds
WHERE
    start_time >= strftime('%s', 'now', '-12 months')
GROUP BY
    year, month
ORDER BY
    year DESC, month DESC;"
        }
        DbType::Postgres => {
            "SELECT
    EXTRACT(YEAR FROM to_timestamp(timestamp_column))::INTEGER AS year,
    EXTRACT(MONTH FROM to_timestamp(timestamp_column))::INTEGER AS month,
    COUNT(*) AS count
FROM
    your_table
WHERE
    timestamp_column >= extract(epoch FROM now() - interval '12 months')
GROUP BY
    year, month
ORDER BY
    year DESC, month DESC;"
        }
    };

    let result = GraphDataPoint::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        query,
        vec![],
    ))
    .all(db)
    .await?;

    Ok(result)
}

async fn get_stats(db: &DatabaseConnection) -> anyhow::Result<ListStats> {
    // Count total builds
    let total_builds: u32 = Builds::find().count(db).await?.try_into()?;

    // Count failed builds
    let failed_builds: u32 = Builds::find()
        .filter(builds::Column::Status.eq(BuildStates::FAILED_BUILD))
        .count(db)
        .await?
        .try_into()?;

    // Count active builds
    let successful_builds: u32 = Builds::find()
        .filter(builds::Column::Status.eq(BuildStates::SUCCESSFUL_BUILD))
        .count(db)
        .await?
        .try_into()?;

    // Calculate repo storage size
    let repo_size: u64 = dir_size("repo/").unwrap_or(0);

    #[derive(Debug, FromQueryResult)]
    struct BuildTimeStruct {
        avg_build_time: Option<BigDecimal>,
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

    let avg_build_time = unique
        .avg_build_time
        .unwrap_or(BigDecimal::try_from(0.0)?)
        .to_u32()
        .unwrap();

    // Count total packages
    let total_packages: u32 = Packages::find().count(db).await?.try_into()?;

    Ok(ListStats {
        total_builds,
        successful_builds,
        failed_builds,
        avg_build_time,
        repo_size,
        total_packages,
        total_build_trend: 0.0,
        total_packages_trend: 0.0,
        repo_size_trend: -0.0,
        avg_build_time_trend: 0.0,
        build_success_trend: 0.0,
    })
}
