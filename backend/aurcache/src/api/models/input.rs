use rocket::serde::{Deserialize, Serialize};
use sea_orm::FromQueryResult;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct ApiPackage {
    pub name: String,
    pub version: String,
}

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize)]
pub struct SimplePackageModel {
    pub id: i32,
    pub name: String,
    pub status: i32,
    pub outofdate: i32,
    pub latest_version: Option<String>,
    pub latest_aur_version: String,
}

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize, Default)]
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
    pub selected_build_flags: Option<Vec<String>>,
    pub aur_url: String,
    pub project_url: Option<String>,
    pub description: Option<String>,
}

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize, Default)]
pub struct PackagePatchModel {
    pub name: Option<String>,
    pub status: Option<i32>,
    pub out_of_date: Option<i32>,
    pub version: Option<Option<String>>,
    pub latest_aur_version: Option<Option<String>>,
    pub latest_build: Option<Option<i32>>,
    pub build_flags: Option<Vec<String>>,
    pub platforms: Option<Vec<String>>,
}

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

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize)]
pub struct ListStats {
    pub total_builds: u32,
    pub successful_builds: u32,
    pub failed_builds: u32,
    pub avg_build_time: u32,
    pub repo_size: u64,
    pub total_packages: u32,

    pub total_build_trend: f32,
    pub avg_build_time_trend: f32,
}

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize)]
pub struct GraphDataPoint {
    pub month: i32,
    pub year: i32,
    pub count: i32,
}

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize)]
pub struct UserInfo {
    pub username: Option<String>,
}
