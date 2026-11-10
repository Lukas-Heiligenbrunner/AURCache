use crate::auth::Authenticated;
use crate::utils::error::ApiError;
use aurcache_common::api::stats::{DashboardView, GraphDataPoint, ListStats};
use rocket::serde::json::Json;
use rocket::{State, get};
use sea_orm::DatabaseConnection;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(stats, dashboard, graph))]
pub struct StatsApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Get general build-server stats", body = ListStats),
    )
)]
#[get("/stats")]
pub async fn stats(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<ListStats>, ApiError> {
    Ok(Json(aurcache_utils::stats::list_stats(db).await?))
}

#[utoipa::path(
    responses(
        (status = 200, description = "Get graph data for dashboard", body = Vec<GraphDataPoint>),
    )
)]
#[get("/graph")]
pub async fn graph(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<Vec<GraphDataPoint>>, ApiError> {
    Ok(Json(aurcache_utils::stats::graph(db).await?))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Dashboard slices for the landing page", body = DashboardView),
    )
)]
#[get("/stats/dashboard")]
pub async fn dashboard(db: &State<DatabaseConnection>, _a: Authenticated) -> Json<DashboardView> {
    Json(aurcache_utils::stats::dashboard(db).await)
}
