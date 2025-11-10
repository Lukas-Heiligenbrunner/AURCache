use aurcache_db::packages::{SourceData, SourceType};
use rocket::serde::{Deserialize, Serialize};
use sea_orm::FromQueryResult;
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema, Clone)]
#[serde(crate = "rocket::serde")]
pub struct AddPackage {
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

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize)]
pub struct SimplePackageModel {
    pub id: i32,
    pub name: String,
    pub status: i32,
    pub outofdate: i32,
    pub latest_version: Option<String>,
    pub latest_aur_version: String,
}

#[derive(Deserialize, ToSchema, Serialize, Clone)]
pub struct ExtendedPackageModel {
    pub id: i32,
    pub name: String,
    pub status: i32,
    pub outofdate: i32,
    pub latest_version: Option<String>,
    pub selected_platforms: Vec<String>,
    pub selected_build_flags: Option<Vec<String>>,
    // todo this should be renamed to "latest_upstream_version" or sth
    pub latest_aur_version: String,

    pub package_source: PackageSource,
    pub package_type: SourceType
}

#[derive(Deserialize, ToSchema, Serialize, Clone)]
#[serde(tag = "package_type", rename_all = "PascalCase")]
pub enum PackageSource {
    Aur(AurPackage),
    Git(GitPackage),
    Upload(UploadPackage),
}

#[derive(Deserialize, ToSchema, Serialize, Default, Clone)]
pub struct GitPackage {
    pub git_url: String,
    pub git_ref: String,
    pub subfolder: String,
}

// todo upload package
#[derive(Deserialize, ToSchema, Serialize, Default, Clone)]
pub struct UploadPackage {}

#[derive(Deserialize, ToSchema, Serialize, Default, Clone)]
pub struct AurPackage {
    pub(crate) name: String,
    pub project_url: Option<String>,
    pub description: Option<String>,
    pub last_updated: u32,
    pub first_submitted: u32,
    pub licenses: Option<String>,
    pub maintainer: Option<String>,
    pub aur_flagged_outdated: bool,
    pub aur_url: String,
}
