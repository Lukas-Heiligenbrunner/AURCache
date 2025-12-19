use aurcache_types::settings::{Setting, SettingsMeta};

pub trait SettingsMetaTrait {
    fn meta(&self) -> SettingsMeta;
}

impl SettingsMetaTrait for Setting {
    fn meta(&self) -> SettingsMeta {
        match self {
            Setting::CpuLimit => SettingsMeta {
                key: "cpu_limit",
                env_name: "CPU_LIMIT",
                default: "0",
            },
            Setting::MemoryLimit => SettingsMeta {
                key: "memory_limit",
                env_name: "MEMORY_LIMIT",
                default: "-1",
            },
            Setting::MaxConcurrentBuilds => SettingsMeta {
                key: "max_concurrent_builds",
                env_name: "MAX_CONCURRENT_BUILDS",
                default: "1",
            },
            Setting::VersionCheckInterval => SettingsMeta {
                key: "version_check_interval",
                env_name: "VERSION_CHECK_INTERVAL",
                default: "3600",
            },
            Setting::AutoUpdateInterval => SettingsMeta {
                key: "auto_update_interval",
                env_name: "AUTO_UPDATE_SCHEDULE",
                default: "", // parses to None
            },
            Setting::JobTimeout => SettingsMeta {
                key: "job_timeout",
                env_name: "JOB_TIMEOUT",
                default: "3600",
            },
            Setting::BuilderImage => SettingsMeta {
                key: "builder_image",
                env_name: "BUILDER_IMAGE",
                default: "ghcr.io/lukas-heiligenbrunner/aurcache-builder:latest",
            },
        }
    }
}
