//! Reading and writing settings.
//!
//! A value resolves `Package -> Env -> Global -> Default`: a value set for the
//! package, then the setting's environment variable, then the value set for the
//! server, then the built-in default. [`key`] names each setting with the type
//! it is read as, so a setting is read as the same type everywhere.

mod parser;

pub use parser::{ByteSize, ParseSetting, Seconds};

use aurcache_common::settings::{
    ApplicationSettings, Scope, Setting, SettingSource, SettingsEntry, SettingsMeta,
};
use aurcache_db::settings;
use sea_orm::{ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use std::marker::PhantomData;

/// A setting, and the type its value is read as.
pub struct Key<T> {
    setting: Setting,
    value: PhantomData<fn() -> T>,
}

impl<T> Key<T> {
    const fn of(setting: Setting) -> Self {
        Self {
            setting,
            value: PhantomData,
        }
    }
}

impl<T> Clone for Key<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Key<T> {}

/// Every setting, typed. The one place a setting's type is stated.
pub mod key {
    use super::{ByteSize, Key, Seconds, Setting};

    pub const VERSION_CHECK_INTERVAL: Key<Seconds> = Key::of(Setting::VersionCheckInterval);
    /// A crontab schedule; empty is off.
    pub const AUTO_UPDATE_SCHEDULE: Key<Option<String>> = Key::of(Setting::AutoUpdateSchedule);
    pub const BUILD_ON_NEW_VERSION: Key<bool> = Key::of(Setting::BuildOnNewVersion);
    pub const PERSISTENT_BUILDDIR: Key<bool> = Key::of(Setting::PersistentBuilddir);
    pub const DATE_FORMAT: Key<String> = Key::of(Setting::DateFormat);
    pub const JOB_TIMEOUT: Key<Seconds> = Key::of(Setting::JobTimeout);
    pub const MAX_ARTIFACT_SIZE: Key<ByteSize> = Key::of(Setting::MaxArtifactSize);
    pub const MAKEPKG_CONF: Key<String> = Key::of(Setting::MakepkgConf);
    pub const PACMAN_CONF: Key<String> = Key::of(Setting::PacmanConf);
    pub const PARSE_NETWORK: Key<bool> = Key::of(Setting::ParseNetwork);
}

/// One setting's value for `pkg_id` (`None` for the whole server), and where it
/// came from.
///
/// A value that does not parse as the key's type falls back to the default,
/// with a warning: a stored value is already there, and a setting nobody can
/// read is worse than one at its default.
pub async fn get<T: ParseSetting>(
    db: &DatabaseConnection,
    key: Key<T>,
    pkg_id: Option<i32>,
) -> SettingsEntry<T> {
    let setting = key.setting.meta();

    let parse_or_default = |val: &str, context: &str| -> T {
        T::parse_setting(val).unwrap_or_else(|e| {
            tracing::warn!(
                "Failed to parse {context}: {e}. Using default '{}'.",
                setting.default
            );
            parse_default(&setting)
        })
    };

    // 1. The value set for the package: explicit intent for one package wins
    //    over the deployment-wide environment. Only for a setting a package
    //    can override; anything else has one value for the whole server.
    if let Some(pid) = pkg_id
        && setting.scope == Scope::Package
        && let Some(v) = stored(db, setting.key, Some(pid)).await
    {
        return SettingsEntry {
            value: parse_or_default(&v, &format!("pkg setting {} pkg={pid}", setting.key)),
            source: SettingSource::Package,
        };
    }

    // 2. The environment: the deployment's override of the server-wide value.
    if let Some(env_name) = setting.env_name
        && let Ok(env_value) = std::env::var(env_name)
    {
        return SettingsEntry {
            value: parse_or_default(&env_value, &format!("ENV {env_name}")),
            source: SettingSource::Env,
        };
    }

    // 3. The value set for the server.
    if let Some(v) = stored(db, setting.key, None).await {
        return SettingsEntry {
            value: parse_or_default(&v, &format!("global setting {}", setting.key)),
            source: SettingSource::Global,
        };
    }

    // 4. The built-in default.
    SettingsEntry {
        value: parse_default(&setting),
        source: SettingSource::Default,
    }
}

/// One setting's value as written, whatever its type: for showing and editing
/// it as text.
pub async fn raw(
    db: &DatabaseConnection,
    setting: Setting,
    pkg_id: Option<i32>,
) -> SettingsEntry<String> {
    get(db, Key::of(setting), pkg_id).await
}

/// The value stored for `key` at `pkg_id`, when there is one.
///
/// No row is the common case, not an error; a failed query is reported and
/// read as no row, so resolution carries on to the next source.
async fn stored(db: &DatabaseConnection, key: &str, pkg_id: Option<i32>) -> Option<String> {
    settings::Entity::find()
        .filter(settings::Column::Key.eq(key))
        .filter(settings::Column::PkgId.eq(settings::scope(pkg_id)))
        .one(db)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to read setting {key} for {pkg_id:?}: {e}. Falling back.");
            None
        })
        .and_then(|row| row.value)
}

/// Parse a setting's built-in default. The defaults are compile-time constants
/// chosen to match each setting's type, so a failure here is a programming bug.
fn parse_default<T: ParseSetting>(setting: &SettingsMeta) -> T {
    T::parse_setting(setting.default).unwrap_or_else(|e| {
        panic!(
            "built-in default '{}' for setting {} is not parseable: {e}",
            setting.default, setting.key
        )
    })
}

/// Every setting for `pkg_id`, resolved.
pub async fn all(db: &DatabaseConnection, pkg_id: Option<i32>) -> ApplicationSettings {
    // Independent reads over one pooled connection: serial awaits would pay
    // up to sixteen point queries (package + global each) in turn.
    let (
        version_check_interval,
        auto_update_schedule,
        job_timeout,
        max_artifact_size,
        date_format,
        build_on_new_version,
        persistent_builddir,
        parse_network,
    ) = tokio::join!(
        get(db, key::VERSION_CHECK_INTERVAL, pkg_id),
        get(db, key::AUTO_UPDATE_SCHEDULE, pkg_id),
        get(db, key::JOB_TIMEOUT, pkg_id),
        get(db, key::MAX_ARTIFACT_SIZE, pkg_id),
        get(db, key::DATE_FORMAT, pkg_id),
        get(db, key::BUILD_ON_NEW_VERSION, pkg_id),
        get(db, key::PERSISTENT_BUILDDIR, pkg_id),
        get(db, key::PARSE_NETWORK, pkg_id),
    );
    // Seconds on the wire, as they always were; written as durations.
    let seconds = |entry: SettingsEntry<Seconds>| SettingsEntry {
        value: u32::try_from(entry.value.0).unwrap_or(u32::MAX),
        source: entry.source,
    };
    ApplicationSettings {
        version_check_interval: seconds(version_check_interval),
        auto_update_schedule,
        job_timeout: seconds(job_timeout),
        max_artifact_size: SettingsEntry {
            value: max_artifact_size.value.0,
            source: max_artifact_size.source,
        },
        date_format,
        build_on_new_version,
        persistent_builddir,
        parse_network,
    }
}

/// A value to store for a setting, or `None` to remove the one stored.
pub struct Change {
    pub setting: Setting,
    /// The package it is for; `None` for the whole server.
    pub pkg_id: Option<i32>,
    pub value: Option<String>,
}

/// Store and remove values: removals first, then every new value in one
/// upsert.
pub async fn write(
    db: &DatabaseConnection,
    changes: impl IntoIterator<Item = Change>,
) -> anyhow::Result<()> {
    let mut inserts = Vec::new();
    let mut removals = sea_orm::Condition::any();
    let mut any_removal = false;

    for Change {
        setting,
        pkg_id,
        value,
    } in changes
    {
        let key = setting.meta().key;
        match value {
            Some(v) => inserts.push(settings::ActiveModel {
                key: Set(key.to_string()),
                pkg_id: Set(settings::scope(pkg_id)),
                value: Set(Some(v)),
                ..Default::default()
            }),
            None => {
                any_removal = true;
                removals = removals.add(
                    sea_orm::Condition::all()
                        .add(settings::Column::Key.eq(key))
                        .add(settings::Column::PkgId.eq(settings::scope(pkg_id))),
                );
            }
        }
    }

    if any_removal {
        settings::Entity::delete_many()
            .filter(removals)
            .exec(db)
            .await?;
    }
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

#[cfg(test)]
mod tests {
    use super::{Change, get, key, write};
    use aurcache_common::settings::{Setting, SettingSource};
    use aurcache_db::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

    async fn db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    /// A package value for a setting packages cannot override is not read,
    /// so the answer is the one the server actually uses.
    #[tokio::test]
    async fn a_global_setting_ignores_a_package_value() {
        let db = db().await;
        write(
            &db,
            [Change {
                setting: Setting::JobTimeout,
                pkg_id: Some(1),
                value: Some("2h".to_string()),
            }],
        )
        .await
        .unwrap();
        let entry = get(&db, key::JOB_TIMEOUT, Some(1)).await;
        assert_eq!(entry.source, SettingSource::Default);
    }

    /// One a package may override is read for it.
    #[tokio::test]
    async fn a_package_setting_reads_the_package_value() {
        let db = db().await;
        write(
            &db,
            [Change {
                setting: Setting::MaxArtifactSize,
                pkg_id: Some(1),
                value: Some("40G".to_string()),
            }],
        )
        .await
        .unwrap();
        let entry = get(&db, key::MAX_ARTIFACT_SIZE, Some(1)).await;
        assert_eq!(entry.source, SettingSource::Package);
    }
}
