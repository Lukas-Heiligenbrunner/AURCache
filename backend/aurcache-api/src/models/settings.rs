use rocket::serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Patch request to change settings
#[derive(ToSchema, Deserialize, Serialize, Clone, Debug)]
pub struct ApplicationSettingsPatch {
    pub cpu_limit: Option<Option<u32>>,
    pub memory_limit: Option<Option<i32>>,
    // todo add all the other settings
}
