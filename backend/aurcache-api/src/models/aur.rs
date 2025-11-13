use aur_rs::Package;
use rocket::serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct ApiPackage {
    pub name: String,
    pub version: String,
}

impl From<Package> for ApiPackage {
    fn from(package: Package) -> Self {
        Self {
            name: package.name,
            version: package.version,
        }
    }
}
