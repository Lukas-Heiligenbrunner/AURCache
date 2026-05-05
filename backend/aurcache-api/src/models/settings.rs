use aurcache_types::settings::SettingSource;
use rocket::serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(ToSchema, Deserialize, Serialize, Clone, Debug)]
pub struct SettingValue {
    /// Raw string value to store. Numeric settings should pass the number as a
    /// string. An empty string is a valid value for nullable/optional settings.
    pub value: String,
}

#[derive(ToSchema, Deserialize, Serialize, Clone, Debug)]
pub struct SettingResponse {
    pub value: String,
    pub source: SettingSource,
}
