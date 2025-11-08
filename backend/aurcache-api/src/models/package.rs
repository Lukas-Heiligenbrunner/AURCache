use aurcache_db::packages::SourceData;
use rocket::serde::{Deserialize, Serialize};
use sea_orm::FromQueryResult;
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema)]
#[serde(crate = "rocket::serde")]
pub struct AddPackage {
    pub(crate) name: String,
    pub(crate) platforms: Option<Vec<String>>,
    pub(crate) build_flags: Option<Vec<String>>,
    pub(crate) source: SourceData,
}

#[derive(Deserialize, ToSchema)]
#[serde(crate = "rocket::serde")]
pub struct UpdatePackage {
    pub(crate) force: bool,
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
