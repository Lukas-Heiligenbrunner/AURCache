use crate::settings::definitions::SettingType;
use aurcache_db::settings;
use aurcache_types::settings::{ApplicationSettings, SettingsEntry};
use sea_orm::*;
use std::str::FromStr;

const GLOBAL_PKG_ID: i32 = -1;

pub async fn set_settings_bulk<I>(entries: I, db: &DatabaseConnection) -> anyhow::Result<()>
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

pub async fn get_setting<T>(
    setting_type: SettingType,
    pkg_id: Option<i32>,
    db: &DatabaseConnection,
) -> SettingsEntry<T>
where
    T: FromStr + Clone,
    <T as FromStr>::Err: std::fmt::Display,
{
    let setting = setting_type.get();

    // Helper to parse a string or fallback to default with a warning
    let parse_or_default = |val: &str, context: &str| -> T {
        match val.parse::<T>() {
            Ok(parsed) => parsed,
            Err(e) => {
                eprintln!(
                    "Warning: Failed to parse {}: {}. Using default '{}'.",
                    context, e, setting.default
                );
                T::from_str(setting.default)
                    .map_err(|e| anyhow::anyhow!("Failed to parse setting {} {e}", setting.key))
                    .unwrap() // safe because default is valid
            }
        }
    };

    // ENV variable takes precedence
    if let Ok(env_value) = std::env::var(setting.env_name) {
        return SettingsEntry {
            value: parse_or_default(&env_value, &format!("ENV {}", setting.env_name)),
            env_forced: true,
            default: false,
        };
    }

    // pkg-specific setting
    if let Some(pid) = pkg_id {
        if let Ok(Some(pkg_entry)) = settings::Entity::find()
            .filter(settings::Column::Key.eq(setting.key))
            .filter(settings::Column::PkgId.eq(pid))
            .one(db)
            .await
        {
            if let Some(v) = pkg_entry.value {
                return SettingsEntry {
                    value: parse_or_default(
                        &v,
                        &format!("pkg setting {} pkg={}", setting.key, pid),
                    ),
                    env_forced: false,
                    default: false,
                };
            }
        } else {
            eprintln!(
                "Warning: Failed to fetch pkg-specific setting {} pkg={}. Using default.",
                setting.key, pid
            );
        }
    }

    // global setting
    if let Ok(Some(global)) = settings::Entity::find()
        .filter(settings::Column::Key.eq(setting.key))
        .filter(settings::Column::PkgId.eq(GLOBAL_PKG_ID))
        .one(db)
        .await
    {
        if let Some(v) = global.value {
            return SettingsEntry {
                value: parse_or_default(&v, &format!("global setting {}", setting.key)),
                env_forced: false,
                default: false,
            };
        }
    } else {
        eprintln!(
            "Warning: Failed to fetch global setting {}. Using default.",
            setting.key
        );
    }

    // fallback default -- unwrap fine here checked value with type before
    SettingsEntry {
        value: T::from_str(setting.default)
            .map_err(|e| anyhow::anyhow!("Failed to parse setting {} {e}", setting.key))
            .unwrap(),
        env_forced: false,
        default: true,
    }
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
            cpu_limit: get_setting(SettingType::CpuLimit, pkgid, db).await,
            memory_limit: get_setting(SettingType::MemoryLimit, pkgid, db).await,
        })
    }

    async fn patch<I>(db: &DatabaseConnection, settings: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = (SettingType, Option<i32>, Option<String>)> + Send,
    {
        set_settings_bulk(settings, db).await
    }
}
