#[derive(Clone)]
pub struct SettingsEntry {
    pub key: &'static str,
    pub env_name: &'static str,
    pub default: &'static str,
}

#[derive(Clone)]
pub enum SettingType {
    CpuLimit,
    MemoryLimit,
    MaxConcurrentBuilds,
}

impl SettingType {
    pub const fn get(&self) -> SettingsEntry {
        match self {
            SettingType::CpuLimit => SettingsEntry {
                key: "cpu_limit",
                env_name: "CPU_LIMIT",
                default: "0",
            },
            SettingType::MemoryLimit => SettingsEntry {
                key: "memory_limit",
                env_name: "MEMORY_LIMIT",
                default: "-1",
            },
            SettingType::MaxConcurrentBuilds => SettingsEntry {
                key: "max_concurrent_builds",
                env_name: "MAX_CONCURRENT_BUILDS",
                default: "1",
            },
        }
    }
}
