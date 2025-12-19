use aurcache_types::settings::Setting;
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
    ) -> Vec<(Setting, Option<i32>, Option<String>)> {
        let mut changedsettings = vec![];

        // cpu limit
        if let Some(cpu_limt) = self.cpu_limit {
            changedsettings.push((Setting::CpuLimit, pkgid, cpu_limt.map(|v| v.to_string())))
        }

        // memory limit
        if let Some(memory_limit) = self.memory_limit {
            changedsettings.push((
                Setting::MemoryLimit,
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
    #[serde(default, deserialize_with = "double_option")]
    pub version_check_interval: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option")]
    pub auto_update_interval: Option<Option<Option<u32>>>,
    #[serde(default, deserialize_with = "double_option")]
    pub job_timeout: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option")]
    pub builder_image: Option<Option<String>>,
    // todo add all the other settings
}
