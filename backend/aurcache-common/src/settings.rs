use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Where a setting's resolved value came from in the lookup hierarchy.
///
/// Resolution order (highest precedence first): `Package` → `Env` → `Global` → `Default`.
#[derive(ToSchema, Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SettingSource {
    /// Forced by an environment variable.
    Env,
    /// Stored on the per-package settings row.
    Package,
    /// Stored on the global settings row.
    Global,
    /// No row stored — using the static built-in default.
    Default,
}

#[derive(ToSchema, Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct SettingsEntry<T> {
    pub value: T,
    pub source: SettingSource,
}

#[derive(ToSchema, Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct ApplicationSettings {
    pub version_check_interval: SettingsEntry<u32>,
    /// A crontab schedule; empty when off.
    pub auto_update_schedule: SettingsEntry<Option<String>>,
    pub job_timeout: SettingsEntry<u32>,
    /// Largest package file a worker may upload, in bytes. Written as a size
    /// (`20G`); see [`Setting::MaxArtifactSize`].
    pub max_artifact_size: SettingsEntry<u64>,
    /// Default date format for the web UI. A browser may override it
    /// locally; nothing writes a per-client choice back here.
    pub date_format: SettingsEntry<String>,
    pub build_on_new_version: SettingsEntry<bool>,
    /// Keep this package's build tree between builds instead of starting from
    /// an empty one. See `design/implemented/persistent-build-directory.md`.
    pub persistent_builddir: SettingsEntry<bool>,
    /// Let a PKGBUILD reach the network while the server parses it.
    /// See [`Setting::ParseNetwork`].
    pub parse_network: SettingsEntry<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingsMeta {
    pub key: &'static str,
    pub env_name: Option<&'static str>,
    pub default: &'static str,
    pub scope: Scope,
}

/// Where a setting can be set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// One value for the whole server.
    Global,
    /// A server-wide value that a package may override.
    Package,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    VersionCheckInterval,
    AutoUpdateSchedule,
    BuildOnNewVersion,
    PersistentBuilddir,
    DateFormat,
    JobTimeout,
    MaxArtifactSize,
    MakepkgConf,
    PacmanConf,
    ParseNetwork,
}

/// Keys of settings that no longer exist, which may still be stored or dumped.
///
/// All four are server settings from when the server ran builds itself, and
/// nothing has read any of them since builds moved to workers:
///
/// - `cpu_limit` and `memory_limit` apply where the build runs, so a worker
///   sets its own (`WORKER_BUILD_MEMORY_MAX`, `WORKER_BUILD_CPUS`);
/// - `max_concurrent_builds` is the worker's `WORKER_CONCURRENCY`, and the
///   scheduler counts leases rather than consulting a server setting;
/// - `builder_image` named the container the server built in, which no longer
///   exists. The image a *worker* runs is that machine's own business, and the
///   one the Workers page suggests for enrolling a new worker comes from
///   `AURCACHE_WORKER_IMAGE`.
///
/// A migration deletes their rows, and a restore skips them so an older dump
/// does not bring them back.
pub const RETIRED_SETTING_KEYS: &[&str] = &[
    "cpu_limit",
    "memory_limit",
    "max_concurrent_builds",
    "builder_image",
];

impl Setting {
    /// Every setting there is.
    ///
    /// [`Self::from_key`] is derived from this rather than written out
    /// separately. The two used to be independent lists, and they drifted:
    /// `date_format` and `build_on_new_version` were served by `GET /settings`
    /// but rejected by `PATCH /settings/<key>` as unknown, so neither could be
    /// changed through the API at all.
    pub const ALL: [Self; 10] = [
        Self::VersionCheckInterval,
        Self::AutoUpdateSchedule,
        Self::BuildOnNewVersion,
        Self::PersistentBuilddir,
        Self::DateFormat,
        Self::JobTimeout,
        Self::MaxArtifactSize,
        Self::MakepkgConf,
        Self::PacmanConf,
        Self::ParseNetwork,
    ];

    /// This setting's stable key, environment variable, built-in default and
    /// scope.
    #[must_use]
    pub const fn meta(&self) -> SettingsMeta {
        match self {
            Self::VersionCheckInterval => SettingsMeta {
                key: "version_check_interval",
                env_name: Some("VERSION_CHECK_INTERVAL"),
                default: "1h",
                scope: Scope::Global,
            },
            // Once a day, at a time of this server's own: a cache exists to
            // keep its packages current. Empty turns it off.
            Self::AutoUpdateSchedule => SettingsMeta {
                key: "auto_update_schedule",
                env_name: Some("AUTO_UPDATE_SCHEDULE"),
                default: "@daily",
                scope: Scope::Global,
            },
            // How the web UI writes absolute dates: field order, zero
            // padding, and a 12- or 24-hour clock, e.g. `dmy-nopad-12`. A
            // browser can override it for itself, which is not stored here —
            // this is the shared starting point, not a per-user preference.
            Self::DateFormat => SettingsMeta {
                key: "date_format",
                env_name: Some("DATE_FORMAT"),
                default: "ymd-pad-24",
                scope: Scope::Global,
            },
            // Queue the rebuild the moment a new version is detected, rather
            // than waiting for the `auto_update_schedule` window. The version
            // check is the only thing that knows a package is out of date —
            // including VCS packages whose upstream moved without a pkgver
            // bump — so that is where the build belongs.
            //
            // On by default, for the same reason the auto-update schedule is:
            // what is out of date is rebuilt without anyone asking. Only
            // packages whose last build succeeded are, so one that keeps
            // failing waits for a person rather than retrying every check.
            Self::BuildOnNewVersion => SettingsMeta {
                key: "build_on_new_version",
                env_name: Some("BUILD_ON_NEW_VERSION"),
                default: "true",
                scope: Scope::Global,
            },
            // On by default. This is what an AUR helper on a workstation
            // already does -- paru and yay keep their build trees between
            // builds and trouble is rare -- and the failure that was assumed
            // to make it risky does not exist: `extract_sources` unpacks over
            // the tree and bsdtar overwrites, so `prepare()` always patches
            // pristine sources. The chroot is still rebuilt from a fresh
            // snapshot every time; only the tree survives.
            //
            // Kept as a setting rather than made unconditional so a package
            // that does turn out to mind has a remedy that is not "delete a
            // directory on the worker by hand".
            Self::PersistentBuilddir => SettingsMeta {
                key: "persistent_builddir",
                env_name: Some("PERSISTENT_BUILDDIR"),
                default: "true",
                scope: Scope::Package,
            },
            Self::JobTimeout => SettingsMeta {
                key: "job_timeout",
                env_name: Some("JOB_TIMEOUT"),
                default: "1h",
                scope: Scope::Global,
            },
            // Largest package file a worker may upload, checked per package so
            // one outsized package (an engine, a game) can be allowed more
            // without raising the ceiling for everything. A cap exists at all
            // because the upload lands on the server's disk before anything
            // looks at it. Written as a size, `20G`, the way the worker's own
            // limits are; see `units::parse_size`.
            Self::MaxArtifactSize => SettingsMeta {
                key: "max_artifact_size",
                env_name: Some("MAX_ARTIFACT_SIZE"),
                default: "20G",
                scope: Scope::Package,
            },
            Self::MakepkgConf => SettingsMeta {
                key: "makepkg_conf",
                env_name: None,
                default: "",
                scope: Scope::Package,
            },
            Self::PacmanConf => SettingsMeta {
                key: "pacman_conf",
                env_name: None,
                default: "",
                scope: Scope::Package,
            },
            // Parsing a PKGBUILD runs it, so the parse is confined and denied
            // TCP. A small minority of packages compute `pkgver` from the
            // network while being sourced -- `pkgver=$(curl -s
            // https://api.github.com/...)` and `git ls-remote` are the usual
            // shapes -- and parse to nothing without it. Off by default
            // because it is the rare case, and because the answer a PKGBUILD
            // fetches is under the same control as the PKGBUILD itself.
            Self::ParseNetwork => SettingsMeta {
                key: "parse_network",
                env_name: Some("PARSE_NETWORK"),
                default: "false",
                scope: Scope::Global,
            },
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.meta().key == key)
    }

    /// Whether `value` is one this setting can use, checked before it is stored.
    ///
    /// Reading falls back to the default when a stored value does not parse,
    /// which is right for a value that is already there and wrong for one being
    /// saved: `max_artifact_size = 40GiB` misspelled as `40 gigs` would quietly
    /// mean 20G. Only the settings that are not plain numbers or free text are
    /// checked here; the rest parse as they always did.
    ///
    /// # Errors
    /// A message saying what was expected.
    pub fn validate(&self, value: &str) -> Result<(), String> {
        match self {
            Self::BuildOnNewVersion | Self::PersistentBuilddir | Self::ParseNetwork => {
                parse_boolean(value).map(|_| ())
            }
            Self::MaxArtifactSize => crate::units::parse_size(value)
                .filter(|&bytes| bytes > 0)
                .map(|_| ())
                .ok_or_else(|| {
                    format!("{value:?} is not a size (expected e.g. 20G, 512M or a byte count)")
                }),
            // Zero is refused rather than stored: a check every zero seconds
            // spins, and a zero timeout reclaims every build as it starts.
            Self::VersionCheckInterval | Self::JobTimeout => crate::units::parse_duration(value)
                .filter(|&seconds| seconds > 0)
                .map(|_| ())
                .ok_or_else(|| {
                    format!(
                        "{value:?} is not a duration (expected e.g. 1h, 90m or a number of seconds)"
                    )
                }),
            // Empty or `@never` is "off". The seed only moves `H` around,
            // never makes a schedule valid or not, so any will do here.
            Self::AutoUpdateSchedule if crate::schedule::is_off(value) => Ok(()),
            Self::AutoUpdateSchedule => crate::schedule::Schedule::parse(value, 0)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Self::DateFormat | Self::MakepkgConf | Self::PacmanConf => Ok(()),
        }
    }
}

/// Parse a boolean setting, including the spellings used in environment variables.
pub fn parse_boolean(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" | "" => Ok(false),
        other => Err(format!("expected a boolean, got {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boolean_settings_reject_values_the_runtime_cannot_read() {
        for setting in [
            Setting::BuildOnNewVersion,
            Setting::PersistentBuilddir,
            Setting::ParseNetwork,
        ] {
            for valid in [
                "true", "false", " TRUE ", "1", "0", "yes", "no", "on", "off", "",
            ] {
                assert_eq!(setting.validate(valid), Ok(()));
            }
            for invalid in ["ture", "enabled", "2"] {
                assert!(setting.validate(invalid).is_err());
            }
        }
    }

    /// A duration is checked before it is stored, and zero is refused: it
    /// would spin the version check, or reclaim every build as it starts.
    #[test]
    fn duration_settings_are_validated() {
        for setting in [Setting::VersionCheckInterval, Setting::JobTimeout] {
            assert_eq!(setting.validate("1h"), Ok(()));
            assert_eq!(setting.validate("3600"), Ok(()));
            assert!(setting.validate("0").is_err());
            assert!(setting.validate("an hour").is_err());
        }
    }

    #[test]
    fn the_schedule_setting_is_validated() {
        let setting = Setting::AutoUpdateSchedule;
        assert_eq!(setting.validate(""), Ok(()));
        assert_eq!(setting.validate("@never"), Ok(()));
        assert_eq!(setting.validate("H 3 * * *"), Ok(()));
        assert!(setting.validate("0 0 3 * * *").is_err());
        assert!(setting.validate("daily").is_err());
    }

    /// The guard on the drift described on [`Setting::ALL`]: a variant left out
    /// of that list is unreachable by key, which is exactly how two settings
    /// became unwritable without anything failing to compile.
    ///
    /// Iterating `ALL` cannot see a variant that is missing *from* `ALL`, so
    /// the position comes from an exhaustive match instead. A new variant does
    /// not compile until it is given a position here, and it then fails this
    /// test until it is added to `ALL` too.
    #[test]
    fn every_setting_is_reachable_by_its_key() {
        fn position(setting: Setting) -> usize {
            match setting {
                Setting::VersionCheckInterval => 0,
                Setting::AutoUpdateSchedule => 1,
                Setting::BuildOnNewVersion => 2,
                Setting::PersistentBuilddir => 3,
                Setting::DateFormat => 4,
                Setting::JobTimeout => 5,
                Setting::MaxArtifactSize => 6,
                Setting::MakepkgConf => 7,
                Setting::PacmanConf => 8,
                Setting::ParseNetwork => 9,
            }
        }

        let mut listed = [false; Setting::ALL.len()];
        for setting in Setting::ALL {
            let key = setting.meta().key;
            let found = Setting::from_key(key).unwrap_or_else(|| panic!("{key} is not in ALL"));
            assert_eq!(found.meta().key, key);
            listed[position(setting)] = true;
        }
        assert!(
            listed.iter().all(|&seen| seen),
            "a setting is missing from Setting::ALL"
        );
    }

    #[test]
    fn keys_and_env_names_are_unique() {
        let mut keys: Vec<_> = Setting::ALL.iter().map(|s| s.meta().key).collect();
        let count = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), count, "two settings share a key");

        let mut envs: Vec<_> = Setting::ALL
            .iter()
            .filter_map(|s| s.meta().env_name)
            .collect();
        let count = envs.len();
        envs.sort_unstable();
        envs.dedup();
        assert_eq!(
            envs.len(),
            count,
            "two settings share an environment variable"
        );
    }
}
