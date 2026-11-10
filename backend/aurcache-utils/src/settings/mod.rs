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
use std::collections::HashMap;
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
) -> Result<SettingsEntry<T>, sea_orm::DbErr> {
    // The environment outranks everything stored but a package's own value,
    // so when no package value can apply and the environment has one there is
    // nothing to read: the answer does not wait on, or fail with, the database.
    let stored = if env_decides(key.setting, pkg_id) {
        Stored::nothing(pkg_id)
    } else {
        Stored::one(db, key.setting, pkg_id).await?
    };
    Ok(resolve(&stored, key))
}

/// Whether the environment answers for `setting` whatever is stored.
fn env_decides(setting: Setting, pkg_id: Option<i32>) -> bool {
    let meta = setting.meta();
    let package_may_override = pkg_id.is_some() && meta.scope == Scope::Package;
    !package_may_override
        && meta
            .env_name
            .is_some_and(|name| std::env::var(name).is_ok())
}

/// The values stored for the server and for one package, as one read saw
/// them.
struct Stored {
    pkg_id: Option<i32>,
    package: HashMap<String, String>,
    global: HashMap<String, String>,
}

impl Stored {
    /// What is stored for `setting`, for the server and for `pkg_id`.
    async fn one(
        db: &DatabaseConnection,
        setting: Setting,
        pkg_id: Option<i32>,
    ) -> Result<Self, sea_orm::DbErr> {
        Self::load(db, Some(setting), pkg_id).await
    }

    /// What is stored for every setting, for the server and for `pkg_id`.
    async fn every(db: &DatabaseConnection, pkg_id: Option<i32>) -> Result<Self, sea_orm::DbErr> {
        Self::load(db, None, pkg_id).await
    }

    /// Nothing stored, as when the database need not be asked.
    fn nothing(pkg_id: Option<i32>) -> Self {
        Self {
            pkg_id,
            package: HashMap::new(),
            global: HashMap::new(),
        }
    }

    /// A failed read must reach the caller: a missing row permits fallback, a
    /// broken database does not.
    async fn load(
        db: &DatabaseConnection,
        setting: Option<Setting>,
        pkg_id: Option<i32>,
    ) -> Result<Self, sea_orm::DbErr> {
        let scopes = std::iter::once(settings::scope(None))
            .chain(pkg_id.map(|id| settings::scope(Some(id))));
        let mut query = settings::Entity::find().filter(settings::Column::PkgId.is_in(scopes));
        if let Some(setting) = setting {
            query = query.filter(settings::Column::Key.eq(setting.meta().key));
        }
        let mut stored = Self::nothing(pkg_id);
        for row in query.all(db).await? {
            let Some(value) = row.value else { continue };
            match settings::package_of(row.pkg_id) {
                Some(_) => stored.package.insert(row.key, value),
                None => stored.global.insert(row.key, value),
            };
        }
        Ok(stored)
    }
}

/// `key`'s value for the package `stored` was read for, from what is stored:
/// `Package -> Env -> Global -> Default`.
fn resolve<T: ParseSetting>(stored: &Stored, key: Key<T>) -> SettingsEntry<T> {
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
    if let Some(pid) = stored.pkg_id
        && setting.scope == Scope::Package
        && let Some(v) = stored.package.get(setting.key)
    {
        return SettingsEntry {
            value: parse_or_default(v, &format!("pkg setting {} pkg={pid}", setting.key)),
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
    if let Some(v) = stored.global.get(setting.key) {
        return SettingsEntry {
            value: parse_or_default(v, &format!("global setting {}", setting.key)),
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
) -> Result<SettingsEntry<String>, sea_orm::DbErr> {
    get(db, Key::of(setting), pkg_id).await
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

/// Every setting for `pkg_id`, resolved from one read of what is stored.
pub async fn all(
    db: &DatabaseConnection,
    pkg_id: Option<i32>,
) -> Result<ApplicationSettings, sea_orm::DbErr> {
    let stored = Stored::every(db, pkg_id).await?;
    // Seconds on the wire, as they always were; written as durations.
    let seconds = |entry: SettingsEntry<Seconds>| SettingsEntry {
        value: u32::try_from(entry.value.0).unwrap_or(u32::MAX),
        source: entry.source,
    };
    Ok(ApplicationSettings {
        version_check_interval: seconds(resolve(&stored, key::VERSION_CHECK_INTERVAL)),
        auto_update_schedule: resolve(&stored, key::AUTO_UPDATE_SCHEDULE),
        job_timeout: seconds(resolve(&stored, key::JOB_TIMEOUT)),
        max_artifact_size: {
            let entry = resolve(&stored, key::MAX_ARTIFACT_SIZE);
            SettingsEntry {
                value: entry.value.0,
                source: entry.source,
            }
        },
        date_format: resolve(&stored, key::DATE_FORMAT),
        build_on_new_version: resolve(&stored, key::BUILD_ON_NEW_VERSION),
        persistent_builddir: resolve(&stored, key::PERSISTENT_BUILDDIR),
        parse_network: resolve(&stored, key::PARSE_NETWORK),
    })
}

/// A value to store for a setting, or `None` to remove the one stored.
pub struct Change {
    pub setting: Setting,
    /// The package it is for; `None` for the whole server.
    pub pkg_id: Option<i32>,
    pub value: Option<String>,
}

/// What [`write`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// Every change is stored.
    Written,
    /// A value was given for a package, for a setting that applies to the whole
    /// server; nothing was stored.
    GlobalOnly(Setting),
}

/// Store and remove values: removals first, then every new value in one
/// upsert.
///
/// All or nothing: a package value for a server-wide setting refuses the
/// whole set, since nothing would ever read it. Removing one stays allowed --
/// it is how a stray row goes.
pub async fn write<C: sea_orm::ConnectionTrait>(
    db: &C,
    changes: impl IntoIterator<Item = Change>,
) -> anyhow::Result<Write> {
    let changes: Vec<Change> = changes.into_iter().collect();
    if let Some(change) = changes.iter().find(|change| {
        change.pkg_id.is_some()
            && change.value.is_some()
            && change.setting.meta().scope == Scope::Global
    }) {
        return Ok(Write::GlobalOnly(change.setting));
    }

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
    Ok(Write::Written)
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

    #[tokio::test]
    async fn unavailable_storage_is_not_a_default_setting() {
        let db = db().await;
        db.clone().close().await.unwrap();
        assert!(get(&db, key::MAX_ARTIFACT_SIZE, Some(1)).await.is_err());
        assert!(
            super::raw(&db, Setting::MaxArtifactSize, None)
                .await
                .is_err()
        );
        assert!(super::all(&db, None).await.is_err());
    }

    /// A package value for a setting packages cannot override is not read,
    /// so the answer is the one the server actually uses.
    #[tokio::test]
    async fn a_global_setting_ignores_a_package_value() {
        let db = db().await;
        write(
            &db,
            [Change {
                setting: Setting::VersionCheckInterval,
                pkg_id: Some(1),
                value: Some("2h".to_string()),
            }],
        )
        .await
        .unwrap();
        let entry = get(&db, key::VERSION_CHECK_INTERVAL, Some(1))
            .await
            .unwrap();
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
        let entry = get(&db, key::MAX_ARTIFACT_SIZE, Some(1)).await.unwrap();
        assert_eq!(entry.source, SettingSource::Package);
    }

    /// The whole page and one setting at a time give the same answers: they
    /// share one resolver, over one read.
    #[tokio::test]
    async fn all_agrees_with_get() {
        let db = db().await;
        write(
            &db,
            [
                Change {
                    setting: Setting::MaxArtifactSize,
                    pkg_id: Some(1),
                    value: Some("40G".to_string()),
                },
                Change {
                    setting: Setting::MaxArtifactSize,
                    pkg_id: None,
                    value: Some("2G".to_string()),
                },
                Change {
                    setting: Setting::DateFormat,
                    pkg_id: None,
                    value: Some("%Y".to_string()),
                },
            ],
        )
        .await
        .unwrap();
        for pkg_id in [None, Some(1), Some(2)] {
            let page = super::all(&db, pkg_id).await.unwrap();
            let size = get(&db, key::MAX_ARTIFACT_SIZE, pkg_id).await.unwrap();
            assert_eq!(page.max_artifact_size.value, size.value.0);
            assert_eq!(page.max_artifact_size.source, size.source);
            let date = get(&db, key::DATE_FORMAT, pkg_id).await.unwrap();
            assert_eq!(page.date_format.value, date.value);
            assert_eq!(page.date_format.source, SettingSource::Global);
        }
        let package = super::all(&db, Some(1)).await.unwrap();
        assert_eq!(package.max_artifact_size.source, SettingSource::Package);
        let other = super::all(&db, Some(2)).await.unwrap();
        assert_eq!(other.max_artifact_size.source, SettingSource::Global);
    }
}
