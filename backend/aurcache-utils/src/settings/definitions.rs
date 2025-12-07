use crate::settings::types::SettingType;
use phf::phf_map;

#[derive(Clone)]
pub struct SettingsEntry {
    pub key: &'static str,
    pub env_name: &'static str,
    pub default: &'static str,
}

impl SettingType {
    const fn as_key(&self) -> &'static str {
        match self {
            SettingType::CpuLimit => "cpu_limit",
            SettingType::MemoryLimit => "memory_limit",
        }
    }
}

pub static SETTINGS: phf::Map<&'static str, SettingsEntry> = phf_map! {
    "cpu_limit" => SettingType::CpuLimit.get(),
    "memory_limit" => SettingType::MemoryLimit.get(),
};
