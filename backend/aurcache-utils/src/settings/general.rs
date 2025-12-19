use crate::settings::types::SettingType;
use aurcache_db::settings;
use aurcache_types::settings::{ApplicationSettings, SettingsEntry};
use sea_orm::*;
use std::str::FromStr;

const GLOBAL_PKG_ID: i32 = -1;

pub async fn set_settings_bulk<I>(
    entries: I,
    db: &DatabaseConnection,
) -> anyhow::Result<()>
where
    I: IntoIterator<Item = (SettingType, Option<i32>, Option<String>)>,
{
    let mut inserts = Vec::new();
    let mut deletes = Vec::new();

    for (st, pkg_id, value) in entries {
        let s = st.get();
        let internal_pkg_id = pkg_id.unwrap_or(GLOBAL_PKG_ID); // Use -1 for global

        match value {
            Some(v) => {
                inserts.push(settings::ActiveModel {
                    key: Set(s.key.to_string()),
                    pkg_id: Set(Some(internal_pkg_id)),
                    value: Set(Some(v)),
                    ..Default::default()
                });
            }
            None => {
                deletes.push((s.key.to_string(), internal_pkg_id));
            }
        }
    }

    // 1️⃣ DELETE overrides
    if !deletes.is_empty() {
        let mut condition = sea_orm::Condition::any();

        for (key, pid) in deletes {
            let c = sea_orm::Condition::all()
                .add(settings::Column::Key.eq(key))
                .add(settings::Column::PkgId.eq(pid));

            condition = condition.add(c);
        }

        settings::Entity::delete_many()
            .filter(condition)
            .exec(db)
            .await?;
    }

    // 2️⃣ UPSERT remaining values
    if !inserts.is_empty() {
        settings::Entity::insert_many(inserts)
            .on_conflict(
                sea_orm::sea_query::OnConflict::columns([
                    settings::Column::Key,
                    settings::Column::PkgId,
                ])
                    .update_column(settings::Column::Value)
                    .to_owned(),
            )
            .exec(db)
            .await?;
    }

    Ok(())
}

/// Priority:
///   1. ENV variable
///   2. global setting (pkg_id = -1 internally)
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

    // global setting
    if let Some(global) = settings::Entity::find()
        .filter(settings::Column::Key.eq(setting.key))
        .filter(settings::Column::PkgId.eq(GLOBAL_PKG_ID))
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

    // pkg-specific setting
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

pub trait SettingsTraits {
    fn get(
        db: &DatabaseConnection,
        pkgid: Option<i32>,
    ) -> impl Future<Output = anyhow::Result<ApplicationSettings>> + Send;
    fn patch<I>(
        db: &DatabaseConnection,
        settings: I,
    ) -> impl Future<Output = anyhow::Result<()>> + Send
    where
        I: IntoIterator<Item = (SettingType, Option<i32>, Option<String>)> + Send;
}

impl SettingsTraits for ApplicationSettings {
    async fn get(
        db: &DatabaseConnection,
        pkgid: Option<i32>,
    ) -> anyhow::Result<ApplicationSettings> {
        Ok(ApplicationSettings {
            cpu_limit: get_setting(SettingType::CpuLimit, pkgid, db).await?,
            memory_limit: get_setting(SettingType::MemoryLimit, pkgid, db).await?,
        })
    }

    async fn patch<I>(
        db: &DatabaseConnection,
        settings: I,
    ) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = (SettingType, Option<i32>, Option<String>)> + Send,
    {
        set_settings_bulk(settings, db).await
    }
}
