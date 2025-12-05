use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(ToSchema, Deserialize, Serialize, Clone, Debug)]
pub struct SettingsEntry<T> {
    pub value: T,
    pub env_forced: bool,
    pub default: bool,
}

#[derive(ToSchema, Deserialize, Serialize, Clone, Debug)]
pub struct ApplicationSettings {
    pub cpu_limit: SettingsEntry<u32>,
    pub memory_limit: SettingsEntry<i32>,
    // todo add all the other settings
}