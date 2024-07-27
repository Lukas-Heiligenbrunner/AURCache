use rocket::serde::{Deserialize, Serialize};
use rocket_okapi::okapi::schemars;
use rocket_okapi::JsonSchema;
use sea_orm::FromQueryResult;

#[derive(Serialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct ApiPackage {
    pub name: String,
    pub version: String,
}

#[derive(FromQueryResult, Deserialize, JsonSchema, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct ListPackageModel {
    pub id: i32,
    pub name: String,
    pub status: i32,
    pub outofdate: i32,
    pub latest_version: Option<String>,
    pub latest_aur_version: String,
}

#[derive(FromQueryResult, Deserialize, JsonSchema, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct ListBuildsModel {
    id: i32,
    pkg_id: i32,
    pkg_name: String,
    version: String,
    status: i32,
    start_time: Option<i64>,
    end_time: Option<i64>,
}

#[derive(FromQueryResult, Deserialize, JsonSchema, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct ListStats {
    pub total_builds: u32,
    pub failed_builds: u32,
    pub avg_queue_wait_time: u32,
    pub avg_build_time: u32,
    pub repo_storage_size: u64,
    pub enqueued_builds: u32,
    pub total_packages: u32,
}
