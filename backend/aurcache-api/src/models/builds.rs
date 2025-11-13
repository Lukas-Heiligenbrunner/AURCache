use rocket::serde::{Deserialize, Serialize};
use sea_orm::FromQueryResult;
use utoipa::ToSchema;

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize)]
pub struct ListBuildsModel {
    id: i32,
    pkg_id: i32,
    pkg_name: String,
    version: String,
    status: i32,
    start_time: Option<i64>,
    end_time: Option<i64>,
    platform: String,
}
