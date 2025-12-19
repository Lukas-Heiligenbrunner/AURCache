use aurcache_utils::settings::definitions::SettingType;
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

impl ApplicationSettingsPatch {
    pub fn get_changed_settings(
        &self,
        pkgid: Option<i32>,
    ) -> Vec<(SettingType, Option<i32>, Option<String>)> {
        let mut changedsettings = vec![];

        // cpu limit
        if let Some(cpu_limt) = self.cpu_limit {
            changedsettings.push((
                SettingType::CpuLimit,
                pkgid,
                cpu_limt.map(|v| v.to_string()),
            ))
        }

        // memory limit
        if let Some(memory_limit) = self.memory_limit {
            changedsettings.push((
                SettingType::MemoryLimit,
                pkgid,
                memory_limit.map(|v| v.to_string()),
            ))
        }
        changedsettings
    }
}

/// Patch request to change settings
#[derive(ToSchema, Deserialize, Serialize, Clone, Debug)]
pub struct ApplicationSettingsPatch {
    #[serde(default, deserialize_with = "double_option")]
    pub cpu_limit: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option")]
    pub memory_limit: Option<Option<i32>>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_concurrent_builds: Option<Option<u32>>,
    // todo add all the other settings
}
