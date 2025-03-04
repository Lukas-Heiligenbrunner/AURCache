use crate::activity_log::activity_utils::{Activity, ActivityLog};
use crate::api::models::authenticated::Authenticated;
use rocket::response::status::NotFound;
use rocket::serde::json::Json;
use rocket::{State, get};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(activity))]
pub struct ActivityApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Get last n Activity entries", body = [Vec<Activity>]),
    )
)]
#[get("/activity?<limit>")]
pub async fn activity(
    _a: Authenticated,
    al: &State<ActivityLog>,
    limit: Option<u64>,
) -> Result<Json<Vec<Activity>>, NotFound<String>> {
    let activities = al.list(limit).await;
    Ok(Json(activities.map_err(|e| NotFound(e.to_string()))?))
}
