use rocket::serde::Deserialize;
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema)]
#[serde(crate = "rocket::serde")]
pub struct AddBody {
    pub(crate) name: String,
    pub(crate) platforms: Option<Vec<String>>,
    pub(crate) build_flags: Option<Vec<String>>,
}

#[derive(Deserialize, ToSchema)]
#[serde(crate = "rocket::serde")]
pub struct AddCustomBody {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) pkgbuild_content: String,
    pub(crate) platforms: Option<Vec<String>>,
    pub(crate) build_flags: Option<Vec<String>>,
}

#[derive(Deserialize, ToSchema)]
#[serde(crate = "rocket::serde")]
pub struct UpdateCustomBody {
    pub(crate) version: String,
    pub(crate) pkgbuild_content: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(crate = "rocket::serde")]
pub struct UpdateBody {
    pub(crate) force: bool,
}
