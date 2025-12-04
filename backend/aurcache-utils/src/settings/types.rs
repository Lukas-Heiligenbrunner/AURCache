pub struct SettingsEntry<T> {
    pub value: T,
    pub env_forced: bool,
    pub default: bool,
}

#[derive(Clone)]
pub enum SettingType {
    CpuLimit,
    MemoryLimit,
}

impl SettingType {
    pub const fn get(&self) -> crate::settings::definitions::SettingsEntry {
        match self {
            SettingType::CpuLimit => crate::settings::definitions::SettingsEntry {
                key: "cpu_limit",
                env_name: "CPU_LIMIT",
                default: "0",
            },
            SettingType::MemoryLimit => crate::settings::definitions::SettingsEntry {
                key: "memory_limit",
                env_name: "MEMORY_LIMIT",
                default: "-1",
            },
        }
    }
}
