use rocket::serde::{Deserialize, Serialize};
use serde::Deserializer;
use utoipa::ToSchema;

fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

/// Patch request to change settings
#[derive(ToSchema, Deserialize, Serialize, Clone, Debug)]
pub struct ApplicationSettingsPatch {
    #[serde(default, deserialize_with = "double_option")]
    pub cpu_limit: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option")]
    pub memory_limit: Option<Option<i32>>,
    // todo add all the other settings
}
