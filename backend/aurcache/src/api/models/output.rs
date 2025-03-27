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
pub struct UpdateBody {
    pub(crate) force: bool,
}
