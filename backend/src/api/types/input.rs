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
pub struct SimplePackageModel {
    pub id: i32,
    pub name: String,
    pub status: i32,
    pub outofdate: i32,
    pub latest_version: Option<String>,
    pub latest_aur_version: String,
}

#[derive(FromQueryResult, Deserialize, JsonSchema, Serialize, Default)]
#[serde(crate = "rocket::serde")]
pub struct ExtendedPackageModel {
    pub id: i32,
    pub name: String,
    pub status: i32,
    pub outofdate: i32,
    pub latest_version: Option<String>,
    pub latest_aur_version: String,
    pub last_updated: u32,
    pub first_submitted: u32,
    pub licenses: Option<String>,
    pub maintainer: Option<String>,
    pub aur_flagged_outdated: bool,
    pub selected_platforms: Vec<String>,
    pub selected_build_flags: Vec<String>,
    pub aur_url: String,
    pub project_url: Option<String>,
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
    pub successful_builds: u32,
    pub failed_builds: u32,
    pub avg_queue_wait_time: u32,
    pub avg_build_time: u32,
    pub repo_storage_size: u64,
    pub enqueued_builds: u32,
    pub total_packages: u32,
}
