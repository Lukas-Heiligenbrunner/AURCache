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
    pub max_concurrent_builds: SettingsEntry<u32>,
    pub version_check_interval: SettingsEntry<u32>,
    pub auto_update_interval: SettingsEntry<Option<String>>,
    pub job_timeout: SettingsEntry<u32>,
    pub builder_image: SettingsEntry<String>,
}

#[derive(Clone)]
pub struct SettingsMeta {
    pub key: &'static str,
    pub env_name: Option<&'static str>,
    pub default: &'static str,
}

#[derive(Clone, Copy)]
pub enum Setting {
    CpuLimit,
    MemoryLimit,
    MaxConcurrentBuilds,
    VersionCheckInterval,
    AutoUpdateInterval,
    JobTimeout,
    BuilderImage,
    MakepkgConf,
    PacmanConf,
}

impl Setting {
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "cpu_limit" => Some(Self::CpuLimit),
            "memory_limit" => Some(Self::MemoryLimit),
            "max_concurrent_builds" => Some(Self::MaxConcurrentBuilds),
            "version_check_interval" => Some(Self::VersionCheckInterval),
            "auto_update_interval" => Some(Self::AutoUpdateInterval),
            "job_timeout" => Some(Self::JobTimeout),
            "builder_image" => Some(Self::BuilderImage),
            "makepkg_conf" => Some(Self::MakepkgConf),
            "pacman_conf" => Some(Self::PacmanConf),
            _ => None,
        }
    }
}
