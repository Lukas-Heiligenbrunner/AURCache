use crate::settings::types::{SettingType};
use aurcache_db::settings;
use sea_orm::*;
use std::str::FromStr;
use aurcache_types::settings::{ApplicationSettings, SettingsEntry};

/// Priority:
///   1. ENV variable
///   2. global setting (pkg_id NULL)
///   3. pkg-specific setting
///   4. default
pub async fn get_setting<T>(
    setting_type: SettingType,
    pkg_id: Option<i32>,
    db: &DatabaseConnection,
) -> anyhow::Result<SettingsEntry<T>>
where
    T: FromStr + Clone,
    <T as FromStr>::Err: std::fmt::Display,
{
    let setting = setting_type.get();
    // ENV variable takes precedence
    if let Ok(env_value) = std::env::var(setting.env_name) {
        let parsed: T = env_value.parse().map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse ENV {}='{}': {}",
                setting.env_name,
                env_value,
                e
            )
        })?;

        return Ok(SettingsEntry {
            value: parsed,
            env_forced: true,
            default: false,
        });
    }

    // global setting (pkg_id IS NULL)
    if let Some(global) = settings::Entity::find()
        .filter(settings::Column::Key.eq(setting.key))
        .filter(settings::Column::PkgId.is_null())
        .one(db)
        .await?
    {
        if let Some(v) = global.value {
            let parsed: T = v.parse().map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse global setting {}='{}': {}",
                    setting.key,
                    v,
                    e
                )
            })?;

            return Ok(SettingsEntry {
                value: parsed,
                env_forced: false,
                default: false,
            });
        }
    }

    // pkg-specific setting (pkg_id = Some(id))
    if let Some(pid) = pkg_id {
        if let Some(pkg_entry) = settings::Entity::find()
            .filter(settings::Column::Key.eq(setting.key))
            .filter(settings::Column::PkgId.eq(pid))
            .one(db)
            .await?
        {
            if let Some(v) = pkg_entry.value {
                let parsed: T = v.parse().map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to parse pkg setting {} pkg={} val='{}': {}",
                        setting.key,
                        pid,
                        v,
                        e
                    )
                })?;

                return Ok(SettingsEntry {
                    value: parsed,
                    env_forced: false,
                    default: false,
                });
            }
        }
    }

    // default value
    Ok(SettingsEntry {
        value: T::from_str(setting.default)
            .map_err(|e| anyhow::anyhow!("Failed to parse setting {} {e}", setting.key))?,
        env_forced: false,
        default: true,
    })
}

pub trait GetAllSettings {
    fn get(db: &DatabaseConnection, pkgid: Option<i32>) -> impl Future<Output = anyhow::Result<ApplicationSettings>> + Send;
}

impl GetAllSettings for ApplicationSettings{
    async fn get(db: &DatabaseConnection, pkgid: Option<i32>) -> anyhow::Result<ApplicationSettings> {
        Ok(ApplicationSettings {
            cpu_limit: get_setting(SettingType::CpuLimit, pkgid, db).await?,
            memory_limit: get_setting(SettingType::MemoryLimit, pkgid, db).await?,
        })
    }
}