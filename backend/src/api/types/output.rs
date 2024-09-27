use rocket::serde::Deserialize;
use rocket_okapi::okapi::schemars;
use rocket_okapi::JsonSchema;

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct AddBody {
    pub(crate) name: String,
    pub(crate) platforms: Option<Vec<String>>,
    pub(crate) build_flags: Option<Vec<String>>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct UpdateBody {
    pub(crate) force: bool,
}
