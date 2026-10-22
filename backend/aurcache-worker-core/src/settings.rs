//! The settings a worker declares, and how it resolves one.
//!
//! A worker's policy settings are declared here rather than read straight out
//! of the environment at the point of use, so that one table is at once:
//!
//! - what the worker tells the server it accepts ([`WorkerSettings::declare`]),
//! - what it tells the server it is actually running
//!   ([`WorkerSettings::effective`]), and
//! - where its own code reads the value from ([`WorkerSettings::size`] and the
//!   other typed readers).
//!
//! Keeping those together is the point. A `WORKER_BUILDDIR_MAX_BYTES=450G` that
//! did not parse used to mean the 200 GiB default with nothing but a line in a
//! journal to say so; here the same value is reported as rejected, beside the
//! value that stands instead, on a page an operator actually looks at.
//!
//! Each declared setting reads **two** variables, which say different things:
//! `WORKER_CONCURRENCY=4` pins the value on this machine, while
//! `WORKER_CONCURRENCY_DEFAULT=4` is a default the server may override. The
//! server's values arrive as a [`ConfigSnapshot`] and are layered between the
//! two ([`WorkerSettings::with_snapshot`]): pin, then server, then environment
//! default, then built-in default.
//!
//! What must *not* be declared is as deliberate as what is: a worker runs
//! devtools as root, so the variables that decide what a build may reach
//! (`WORKER_BIND_MOUNTS`, `WORKER_BUILD_USER`, `WORKER_MAKECHROOTPKG`) and the
//! ones needed before the server can be trusted at all (`AURCACHE_URL`, the
//! enrollment settings) are not in any spec table. See
//! `design/implemented/worker-configuration.md`.

use aurcache_common::units::{format_duration, format_size, parse_duration, parse_size};
use aurcache_common::worker_config::{
    Applies, ConfigSnapshot, EffectiveConfig, EffectiveSetting, EffectiveSource, SettingDecl,
    SettingStatus, ValueKind,
};
use std::collections::BTreeMap;

use crate::config::env_opt;

/// The suffix that turns a pinning variable into an overridable default.
const DEFAULT_SUFFIX: &str = "_DEFAULT";

/// A setting's built-in default, typed so that the value the worker's code
/// reads and the string the server is shown cannot disagree.
///
/// Rendering the string from the value (rather than storing both) is what keeps
/// "the default is 200 GiB" and "the default reads `200G`" the same statement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Builtin {
    /// No value, where having none is meaningful -- an unlimited budget, an
    /// affinity list that reserves nothing.
    Unset,
    Size(u64),
    /// Seconds.
    Duration(u64),
    Integer(i64),
    Float(f64),
    Bool(bool),
    Text(&'static str),
}

impl Builtin {
    /// The default as configuration would write it, or `None` when unset.
    #[must_use]
    pub fn as_written(&self) -> Option<String> {
        match self {
            Self::Unset => None,
            Self::Size(bytes) => Some(format_size(*bytes)),
            Self::Duration(seconds) => Some(format_duration(*seconds)),
            Self::Integer(value) => Some(value.to_string()),
            Self::Float(value) => Some(value.to_string()),
            Self::Bool(value) => Some(value.to_string()),
            Self::Text(value) => Some((*value).to_string()),
        }
    }
}

/// One setting this worker accepts, as declared in its own code.
#[derive(Clone, Debug)]
pub struct SettingSpec {
    /// Stable identifier the server stores values under.
    pub key: &'static str,
    /// The variable that pins it; `<env_var>_DEFAULT` sets an overridable
    /// default.
    pub env_var: &'static str,
    pub kind: ValueKind,
    pub description: &'static str,
    pub category: &'static str,
    pub applies: Applies,
    pub default: Builtin,
}

/// What one setting resolved to: the value, and where it came from.
#[derive(Clone, Debug, PartialEq)]
struct Resolution {
    /// The value in effect, written as configuration writes it.
    value: Option<String>,
    source: EffectiveSource,
    status: SettingStatus,
    reason: Option<String>,
}

/// A declared setting together with what it resolved to on this machine.
#[derive(Clone, Debug)]
struct Entry {
    spec: SettingSpec,
    /// What it is running now.
    current: Resolution,
    /// What the environment alone resolved it to at startup: the pin, the
    /// environment default, or the built-in one. What a setting returns to
    /// when the server's value for it is removed.
    from_env: Resolution,
    /// The pin, when it was set and usable. Beats anything the server sends,
    /// which is what lets a machine keep a value -- its memory limit, say --
    /// out of the server's hands.
    pin: Option<String>,
    /// What this setting falls back to without a pin: the `_DEFAULT` the worker
    /// was started with when it has a usable one, else the built-in default.
    ///
    /// Reported as the declaration's default so that resetting a value shows
    /// what the worker would actually return to, not a built-in it would not.
    fallback: Option<String>,
}

/// Every setting a worker declares, resolved against its environment and
/// whatever the server has set.
#[derive(Clone, Debug, Default)]
pub struct WorkerSettings {
    entries: Vec<Entry>,
    /// The revision of the last snapshot this was resolved against, or `None`
    /// before the server has sent one.
    revision: Option<String>,
    /// Keys the server sent a value for that this worker does not declare, so
    /// the report can say they were not used rather than leave them out.
    unsupported: Vec<String>,
}

impl WorkerSettings {
    /// Resolve a table of specs against the process environment.
    #[must_use]
    pub fn from_env(specs: Vec<SettingSpec>) -> Self {
        Self::default().extended(specs)
    }

    /// Resolve another table and append it.
    ///
    /// An executor declares its own settings beside the protocol ones, so the
    /// chroot worker and the container worker expose different tables without
    /// either of them knowing about the other's.
    #[must_use]
    pub fn extended(mut self, specs: Vec<SettingSpec>) -> Self {
        self.entries.extend(specs.into_iter().map(resolve));
        self
    }

    /// Resolve again with the server's values layered in: the pin, then the
    /// server's value, then the environment default, then the built-in one.
    ///
    /// Always against the whole snapshot -- it is the complete set, so a key
    /// missing from it is a value the server no longer sets. A server value
    /// that does not parse is refused and the setting keeps what it is running
    /// now: a refusal must never loosen a limit by dropping to a default.
    #[must_use]
    pub fn with_snapshot(&self, snapshot: &ConfigSnapshot) -> Self {
        let entries = self
            .entries
            .iter()
            .map(|entry| {
                let current = match (&entry.pin, snapshot.settings.get(entry.spec.key)) {
                    (Some(_), Some(server)) => Resolution {
                        status: SettingStatus::Overridden,
                        reason: Some(format!(
                            "set to {server} in AURCache, but {var} pins it on this \
                             machine; rename it to {var}{DEFAULT_SUFFIX} to manage it from \
                             AURCache",
                            var = entry.spec.env_var,
                        )),
                        ..entry.from_env.clone()
                    },
                    (Some(_), None) | (None, None) => entry.from_env.clone(),
                    (None, Some(server)) => match entry.spec.kind.validate(server) {
                        Ok(()) => Resolution {
                            value: Some(server.trim().to_string()),
                            source: EffectiveSource::Server,
                            // A pin that did not parse is still worth hearing
                            // about while the server's value stands in for it.
                            status: entry.from_env.status,
                            reason: entry.from_env.reason.clone(),
                        },
                        Err(why) => Resolution {
                            status: SettingStatus::Rejected,
                            reason: Some(format!("the server's value was refused: {why}")),
                            ..entry.current.clone()
                        },
                    },
                };
                Entry {
                    current,
                    ..entry.clone()
                }
            })
            .collect();
        let unsupported = snapshot
            .settings
            .keys()
            .filter(|key| !self.entries.iter().any(|entry| entry.spec.key == *key))
            .cloned()
            .collect();
        Self {
            entries,
            revision: Some(snapshot.revision.clone()),
            unsupported,
        }
    }

    /// Refuse the values an executor could not use, each keeping what
    /// `previous` was running for it.
    ///
    /// For what only the machine can find out -- a CPU limit on a host whose
    /// cgroup cannot take one. The previous value, not a default: a limit
    /// the host refused must not leave the builds with none.
    #[must_use]
    pub fn refused(mut self, refusals: &BTreeMap<String, String>, previous: &Self) -> Self {
        for entry in &mut self.entries {
            let Some(why) = refusals.get(entry.spec.key) else {
                continue;
            };
            let before = previous
                .entries
                .iter()
                .find(|old| old.spec.key == entry.spec.key)
                .map_or_else(|| entry.from_env.clone(), |old| old.current.clone());
            entry.current = Resolution {
                status: SettingStatus::Rejected,
                reason: Some(why.clone()),
                ..before
            };
        }
        self
    }

    /// Where the value in effect came from, for an executor deciding which
    /// values are the server's to refuse.
    #[must_use]
    pub fn source(&self, key: &str) -> Option<EffectiveSource> {
        Some(self.entry(key)?.current.source)
    }

    fn entry(&self, key: &str) -> Option<&Entry> {
        let found = self.entries.iter().find(|entry| entry.spec.key == key);
        if found.is_none() {
            // A key read but never declared is a bug in this crate, not a
            // deployment problem: the readers below and the spec tables are
            // written together.
            tracing::error!("no worker setting is declared as {key:?}");
        }
        found
    }

    /// The value in effect, as written. `None` for a setting that is unset.
    #[must_use]
    pub fn raw(&self, key: &str) -> Option<&str> {
        self.entry(key)?.current.value.as_deref()
    }

    /// The value in effect as a byte count.
    #[must_use]
    pub fn size(&self, key: &str) -> Option<u64> {
        parse_size(self.raw(key)?)
    }

    /// The value in effect as a number of seconds.
    #[must_use]
    pub fn duration(&self, key: &str) -> Option<u64> {
        parse_duration(self.raw(key)?)
    }

    /// The value in effect as a whole number.
    #[must_use]
    pub fn integer(&self, key: &str) -> Option<i64> {
        self.raw(key)?.parse().ok()
    }

    /// The value in effect as a number.
    #[must_use]
    pub fn float(&self, key: &str) -> Option<f64> {
        self.raw(key)?.parse().ok()
    }

    /// The value in effect as a comma- or space-separated list.
    #[must_use]
    pub fn list(&self, key: &str) -> Vec<String> {
        self.raw(key)
            .map(crate::config::parse_list)
            .unwrap_or_default()
    }

    /// What this worker accepts, for the server to store and render.
    #[must_use]
    pub fn declare(&self) -> Vec<SettingDecl> {
        self.entries
            .iter()
            .map(|entry| SettingDecl {
                key: entry.spec.key.to_string(),
                kind: entry.spec.kind.clone(),
                description: entry.spec.description.to_string(),
                category: entry.spec.category.to_string(),
                default: entry.fallback.clone(),
                env_var: Some(entry.spec.env_var.to_string()),
                applies: entry.spec.applies,
            })
            .collect()
    }

    /// What this worker is running, for the server to store and render.
    #[must_use]
    pub fn effective(&self) -> EffectiveConfig {
        let declared = self.entries.iter().map(|entry| {
            let current = &entry.current;
            (
                entry.spec.key.to_string(),
                EffectiveSetting {
                    value: current.value.clone(),
                    source: current.source,
                    status: current.status,
                    reason: current.reason.clone(),
                },
            )
        });
        let unsupported = self.unsupported.iter().map(|key| {
            (
                key.clone(),
                EffectiveSetting {
                    value: None,
                    source: EffectiveSource::Server,
                    status: SettingStatus::Unsupported,
                    reason: Some(format!(
                        "this worker does not accept a setting called {key}"
                    )),
                },
            )
        });
        EffectiveConfig {
            received_revision: self.revision.clone(),
            settings: declared.chain(unsupported).collect::<BTreeMap<_, _>>(),
        }
    }
}

/// Resolve one setting: the pin, then the environment default, then the
/// built-in one, taking the first that is usable.
///
/// A value that does not parse falls through to the next level rather than
/// stopping the worker -- a typo should not keep a machine from building -- but
/// it is reported as rejected, with the value that stands instead, rather than
/// disappearing into a log line.
fn resolve(spec: SettingSpec) -> Entry {
    let default_var = format!("{}{DEFAULT_SUFFIX}", spec.env_var);
    let pin = env_opt(spec.env_var);
    let env_default = env_opt(&default_var);

    // Highest precedence first. A rejection at one level is recorded and the
    // next is tried.
    let candidates = [
        (pin.as_deref(), EffectiveSource::Env, spec.env_var),
        (
            env_default.as_deref(),
            EffectiveSource::EnvDefault,
            default_var.as_str(),
        ),
    ];

    let mut rejected: Option<String> = None;
    let mut chosen: Option<(String, EffectiveSource)> = None;
    for (raw, source, var) in candidates {
        let Some(raw) = raw else { continue };
        // Validated as stored: the stored form is trimmed, so validating the
        // raw value rejects entries over insignificant whitespace.
        let trimmed = raw.trim();
        match spec.kind.validate(trimmed) {
            Ok(()) => {
                chosen = Some((trimmed.to_string(), source));
                break;
            }
            Err(why) => {
                tracing::warn!("ignoring {var}={raw:?} ({why})");
                // The first refusal is the one an operator meant most, so it is
                // the one reported.
                rejected.get_or_insert_with(|| format!("{var} ignored: {why}"));
            }
        }
    }

    // The fallback the declaration advertises: what this worker would use with
    // no pin and nothing from the server.
    let fallback = env_default
        .as_deref()
        .filter(|raw| spec.kind.validate(raw.trim()).is_ok())
        .map(|raw| raw.trim().to_string())
        .or_else(|| spec.default.as_written());

    let pin_value = chosen
        .as_ref()
        .filter(|(_, source)| *source == EffectiveSource::Env)
        .map(|(value, _)| value.clone());
    let (value, source) = chosen.map_or_else(
        || (spec.default.as_written(), EffectiveSource::Default),
        |(value, source)| (Some(value), source),
    );

    // A pin that shadows a default is almost always a half-finished handover,
    // so it is worth saying out loud even though the pin is doing exactly what
    // a pin should.
    let shadowed = (pin.is_some() && env_default.is_some() && source == EffectiveSource::Env)
        .then(|| format!("{default_var} is also set; the pin wins"));

    let status = if rejected.is_some() {
        SettingStatus::Rejected
    } else {
        SettingStatus::Applied
    };

    let resolved = Resolution {
        value,
        source,
        status,
        reason: rejected.or(shadowed),
    };
    Entry {
        spec,
        current: resolved.clone(),
        from_env: resolved,
        pin: pin_value,
        fallback,
    }
}

/// Keys of the settings every executor has, so a reader and the table below
/// cannot drift apart over a typo.
pub mod keys {
    pub const CONCURRENCY: &str = "concurrency";
    pub const PRIORITY: &str = "priority";
    pub const PACKAGES: &str = "packages";
    pub const POLL_INTERVAL: &str = "poll_interval";
    pub const BUILD_TIMEOUT: &str = "build_timeout";
    pub const BUILDDIR_MAX_BYTES: &str = "builddir_max_bytes";
    pub const BUILDDIR_MIN_FREE: &str = "builddir_min_free";
}

/// One build at a time unless asked otherwise.
///
/// Each build is *already* parallel -- the server renders `MAKEFLAGS=-j$(nproc)`
/// into every job -- so defaulting to the core count multiplies out to nproc x
/// nproc compiler processes, each build also holding its own chroot copy and
/// package cache. That is memory exhaustion, not throughput.
pub const DEFAULT_CONCURRENCY: i64 = 1;
/// Enough for one very large build tree, small enough that opting in a second
/// is a decision someone makes.
pub const DEFAULT_BUILDDIR_MAX_BYTES: u64 = 200 * 1024 * 1024 * 1024;
/// Secondary floor, covering what a cap cannot see: a small disk, or one
/// shared with something else that grew.
pub const DEFAULT_BUILDDIR_MIN_FREE: u64 = 50 * 1024 * 1024 * 1024;
/// How long to wait before asking for work again when there is none.
pub const DEFAULT_POLL_INTERVAL: u64 = 10;
/// Long enough for the large packages this exists to build.
pub const DEFAULT_BUILD_TIMEOUT: u64 = 3 * 60 * 60;
/// Keyserver `gpg --recv-keys` asks for a PKGBUILD's `validpgpkeys`.
///
/// One spelling shared by the chroot executor's setting default and the legacy
/// container executor's build script, so both ask the same place. Safe for the
/// server to set, unlike the rest of the signature path: the keys a build will
/// accept are pinned by the PKGBUILD, so a keyserver can withhold a key but
/// cannot substitute one.
pub const DEFAULT_KEYSERVER: &str = "hkps://keyserver.ubuntu.com";

/// The settings every executor has, whatever it builds with.
///
/// Scheduling and liveness, which belong to the protocol rather than to how a
/// package is actually made. An executor adds its own table to this one.
#[must_use]
pub fn protocol_settings() -> Vec<SettingSpec> {
    vec![
        SettingSpec {
            key: keys::CONCURRENCY,
            env_var: "WORKER_CONCURRENCY",
            kind: ValueKind::Integer {
                min: Some(1),
                max: None,
            },
            description: "How many builds this worker runs at once. Each build is already \
                          parallel, so raising this multiplies compiler processes, chroot \
                          copies and cache pressure together.",
            category: "Scheduling",
            // Through the concurrency gate: lowering it lets running builds
            // finish, raising it lets the next claim through at once.
            applies: Applies::Immediately,
            default: Builtin::Integer(DEFAULT_CONCURRENCY),
        },
        SettingSpec {
            key: keys::PRIORITY,
            env_var: "WORKER_PRIORITY",
            kind: ValueKind::Integer {
                min: None,
                max: None,
            },
            description: "Scheduling preference; higher wins. A worker holds back only while a \
                          strictly higher-priority one could take the job.",
            category: "Scheduling",
            applies: Applies::Immediately,
            default: Builtin::Integer(0),
        },
        SettingSpec {
            key: keys::PACKAGES,
            env_var: "WORKER_PACKAGES",
            kind: ValueKind::List,
            description: "Exact pkgbase names this worker is provisioned for. A package named \
                          by any approved worker may only be built by workers that name it, so \
                          this restricts the package as much as it reserves the worker.",
            category: "Scheduling",
            applies: Applies::Immediately,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::POLL_INTERVAL,
            env_var: "WORKER_POLL_INTERVAL",
            kind: ValueKind::Duration,
            description: "How long to wait before asking for work again when the queue is \
                          empty.",
            category: "Scheduling",
            applies: Applies::NextLoop,
            default: Builtin::Duration(DEFAULT_POLL_INTERVAL),
        },
        SettingSpec {
            key: keys::BUILD_TIMEOUT,
            env_var: "WORKER_BUILD_TIMEOUT",
            kind: ValueKind::Duration,
            description: "How long a single build may run before the worker kills it. 0 leaves \
                          it to the server's lease.",
            category: "Build limits",
            applies: Applies::NextJob,
            default: Builtin::Duration(DEFAULT_BUILD_TIMEOUT),
        },
        SettingSpec {
            key: keys::BUILDDIR_MAX_BYTES,
            env_var: "WORKER_BUILDDIR_MAX_BYTES",
            kind: ValueKind::Size,
            description: "Total disk the kept build trees may occupy before the oldest are \
                          swept.",
            category: "Build trees",
            applies: Applies::NextLoop,
            default: Builtin::Size(DEFAULT_BUILDDIR_MAX_BYTES),
        },
        SettingSpec {
            key: keys::BUILDDIR_MIN_FREE,
            env_var: "WORKER_BUILDDIR_MIN_FREE",
            kind: ValueKind::Size,
            description: "Disk to keep free on the filesystem holding the build trees, whatever \
                          the cap above allows.",
            category: "Build trees",
            applies: Applies::NextLoop,
            default: Builtin::Size(DEFAULT_BUILDDIR_MIN_FREE),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolution reads the process environment, which is global to the test
    /// binary, so each case runs against variables named for itself.
    fn spec(env_var: &'static str, kind: ValueKind, default: Builtin) -> SettingSpec {
        SettingSpec {
            key: "under_test",
            env_var,
            kind,
            description: "d",
            category: "c",
            applies: Applies::NextJob,
            default,
        }
    }

    fn resolved(spec: SettingSpec) -> Entry {
        resolve(spec)
    }

    #[test]
    fn nothing_set_uses_the_built_in_default() {
        let entry = resolved(spec(
            "AURCACHE_TEST_UNSET",
            ValueKind::Size,
            Builtin::Size(200 * 1024 * 1024 * 1024),
        ));
        assert_eq!(entry.current.value.as_deref(), Some("200G"));
        assert_eq!(entry.current.source, EffectiveSource::Default);
        assert_eq!(entry.current.status, SettingStatus::Applied);
        assert_eq!(entry.fallback.as_deref(), Some("200G"));
    }

    #[test]
    fn a_plain_variable_pins() {
        unsafe { std::env::set_var("AURCACHE_TEST_PIN", "1T") };
        let entry = resolved(spec(
            "AURCACHE_TEST_PIN",
            ValueKind::Size,
            Builtin::Size(1024),
        ));
        assert_eq!(entry.current.value.as_deref(), Some("1T"));
        assert_eq!(entry.current.source, EffectiveSource::Env);
        unsafe { std::env::remove_var("AURCACHE_TEST_PIN") };
    }

    /// Validated as stored: surrounding whitespace must not reject a value
    /// the trimmed form satisfies.
    #[test]
    fn surrounding_whitespace_does_not_reject_a_valid_value() {
        unsafe { std::env::set_var("AURCACHE_TEST_PADDED", " 4 ") };
        let entry = resolved(spec(
            "AURCACHE_TEST_PADDED",
            ValueKind::Integer {
                min: Some(1),
                max: None,
            },
            Builtin::Integer(1),
        ));
        assert_eq!(entry.current.value.as_deref(), Some("4"));
        assert_eq!(entry.current.status, SettingStatus::Applied);
        unsafe { std::env::remove_var("AURCACHE_TEST_PADDED") };
    }

    /// `_DEFAULT` is used when nothing pins, and is what the declaration
    /// advertises as the fallback -- so resetting a value shows the machine's
    /// own starting point rather than a built-in it would not return to.
    #[test]
    fn an_environment_default_supplies_the_value_and_the_fallback() {
        unsafe { std::env::set_var("AURCACHE_TEST_DEF_DEFAULT", "4") };
        let entry = resolved(spec(
            "AURCACHE_TEST_DEF",
            ValueKind::Integer {
                min: Some(1),
                max: None,
            },
            Builtin::Integer(1),
        ));
        assert_eq!(entry.current.value.as_deref(), Some("4"));
        assert_eq!(entry.current.source, EffectiveSource::EnvDefault);
        assert_eq!(entry.fallback.as_deref(), Some("4"));
        unsafe { std::env::remove_var("AURCACHE_TEST_DEF_DEFAULT") };
    }

    /// Both set is almost certainly a rename left half-done, so the pin wins
    /// and the worker says so.
    #[test]
    fn a_pin_beats_an_environment_default_and_reports_it() {
        unsafe {
            std::env::set_var("AURCACHE_TEST_BOTH", "2");
            std::env::set_var("AURCACHE_TEST_BOTH_DEFAULT", "9");
        }
        let entry = resolved(spec(
            "AURCACHE_TEST_BOTH",
            ValueKind::Integer {
                min: None,
                max: None,
            },
            Builtin::Integer(1),
        ));
        assert_eq!(entry.current.value.as_deref(), Some("2"));
        assert_eq!(entry.current.source, EffectiveSource::Env);
        assert_eq!(entry.current.status, SettingStatus::Applied);
        assert!(
            entry
                .current
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("AURCACHE_TEST_BOTH_DEFAULT")),
            "{:?}",
            entry.current.reason
        );
        unsafe {
            std::env::remove_var("AURCACHE_TEST_BOTH");
            std::env::remove_var("AURCACHE_TEST_BOTH_DEFAULT");
        }
    }

    /// The `450G` that used to mean 200 GiB in silence. It still falls back --
    /// a typo must not stop a worker building -- but the report names the
    /// variable, the value, and what is in force instead.
    #[test]
    fn an_unusable_pin_falls_back_and_is_reported() {
        unsafe { std::env::set_var("AURCACHE_TEST_BAD", "450 giraffes") };
        let entry = resolved(spec(
            "AURCACHE_TEST_BAD",
            ValueKind::Size,
            Builtin::Size(200 * 1024 * 1024 * 1024),
        ));
        assert_eq!(entry.current.value.as_deref(), Some("200G"));
        assert_eq!(entry.current.source, EffectiveSource::Default);
        assert_eq!(entry.current.status, SettingStatus::Rejected);
        let reason = entry.current.reason.expect("a rejection says why");
        assert!(reason.contains("AURCACHE_TEST_BAD"), "{reason}");
        assert!(reason.contains("450 giraffes"), "{reason}");
        unsafe { std::env::remove_var("AURCACHE_TEST_BAD") };
    }

    /// A refused pin drops to the next level down, not straight to the built-in
    /// default: the machine's own `_DEFAULT` is still a value someone chose.
    #[test]
    fn a_refused_pin_falls_to_the_environment_default() {
        unsafe {
            std::env::set_var("AURCACHE_TEST_LADDER", "eight");
            std::env::set_var("AURCACHE_TEST_LADDER_DEFAULT", "8");
        }
        let entry = resolved(spec(
            "AURCACHE_TEST_LADDER",
            ValueKind::Integer {
                min: None,
                max: None,
            },
            Builtin::Integer(1),
        ));
        assert_eq!(entry.current.value.as_deref(), Some("8"));
        assert_eq!(entry.current.source, EffectiveSource::EnvDefault);
        assert_eq!(entry.current.status, SettingStatus::Rejected);
        unsafe {
            std::env::remove_var("AURCACHE_TEST_LADDER");
            std::env::remove_var("AURCACHE_TEST_LADDER_DEFAULT");
        }
    }

    /// An unset default stays unset rather than becoming an empty string: for a
    /// budget or a limit, "nothing configured" means unlimited and is a
    /// different answer from zero.
    #[test]
    fn an_unset_default_reports_no_value() {
        let entry = resolved(spec(
            "AURCACHE_TEST_UNLIMITED",
            ValueKind::Size,
            Builtin::Unset,
        ));
        assert_eq!(entry.current.value, None);
        assert_eq!(entry.fallback, None);
        assert_eq!(entry.current.source, EffectiveSource::Default);
    }

    fn snapshot(revision: &str, pairs: &[(&str, &str)]) -> ConfigSnapshot {
        ConfigSnapshot {
            revision: revision.to_string(),
            settings: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    fn integer(env_var: &'static str, key: &'static str) -> SettingSpec {
        SettingSpec {
            key,
            ..spec(
                env_var,
                ValueKind::Integer {
                    min: Some(1),
                    max: None,
                },
                Builtin::Integer(1),
            )
        }
    }

    /// The server's value beats the machine's `_DEFAULT` and the built-in
    /// default, and removing it returns the setting to exactly what the
    /// environment resolved it to.
    #[test]
    fn a_server_value_beats_an_environment_default_and_can_be_taken_back() {
        unsafe { std::env::set_var("AURCACHE_TEST_SRV_DEFAULT", "4") };
        let settings = WorkerSettings::from_env(vec![integer("AURCACHE_TEST_SRV", "srv")]);
        assert_eq!(settings.raw("srv"), Some("4"));

        let delivered = settings.with_snapshot(&snapshot("r1", &[("srv", "7")]));
        assert_eq!(delivered.raw("srv"), Some("7"));
        assert_eq!(delivered.source("srv"), Some(EffectiveSource::Server));
        assert_eq!(
            delivered.effective().received_revision.as_deref(),
            Some("r1")
        );

        let removed = delivered.with_snapshot(&snapshot("r2", &[]));
        assert_eq!(removed.raw("srv"), Some("4"));
        assert_eq!(removed.source("srv"), Some(EffectiveSource::EnvDefault));
        unsafe { std::env::remove_var("AURCACHE_TEST_SRV_DEFAULT") };
    }

    /// A pin is the machine's veto: the server's value is received, reported
    /// as not in effect, and the report names the rename that hands it over.
    #[test]
    fn a_pin_overrides_the_server_and_says_how_to_hand_it_over() {
        unsafe { std::env::set_var("AURCACHE_TEST_PINNED", "2") };
        let settings = WorkerSettings::from_env(vec![integer("AURCACHE_TEST_PINNED", "pinned")]);
        let delivered = settings.with_snapshot(&snapshot("r1", &[("pinned", "9")]));
        assert_eq!(delivered.raw("pinned"), Some("2"));
        let report = &delivered.effective().settings["pinned"];
        assert_eq!(report.status, SettingStatus::Overridden);
        assert_eq!(report.source, EffectiveSource::Env);
        let reason = report.reason.as_deref().unwrap_or_default();
        assert!(reason.contains("AURCACHE_TEST_PINNED_DEFAULT"), "{reason}");
        unsafe { std::env::remove_var("AURCACHE_TEST_PINNED") };
    }

    /// A server value that does not fit is refused, and the setting keeps what
    /// it was running -- the previous server value, not a looser default.
    #[test]
    fn a_refused_server_value_keeps_the_previous_one() {
        let settings = WorkerSettings::from_env(vec![integer("AURCACHE_TEST_KEEP", "keep")]);
        let first = settings.with_snapshot(&snapshot("r1", &[("keep", "3")]));
        let second = first.with_snapshot(&snapshot("r2", &[("keep", "0")]));
        assert_eq!(second.raw("keep"), Some("3"));
        let report = &second.effective().settings["keep"];
        assert_eq!(report.status, SettingStatus::Rejected);
        assert_eq!(report.source, EffectiveSource::Server);
        // Received all the same: resending it would not change the answer.
        assert_eq!(second.effective().received_revision.as_deref(), Some("r2"));
    }

    /// A key the worker does not declare is reported as not used rather than
    /// dropped, and never becomes a setting it runs.
    #[test]
    fn an_undeclared_key_is_reported_unsupported() {
        let settings = WorkerSettings::from_env(vec![integer("AURCACHE_TEST_DECL", "decl")]);
        let delivered = settings.with_snapshot(&snapshot("r1", &[("build_user", "root")]));
        let report = &delivered.effective().settings["build_user"];
        assert_eq!(report.status, SettingStatus::Unsupported);
        assert_eq!(report.value, None);
        // Gone again once the server stops sending it.
        let later = delivered.with_snapshot(&snapshot("r2", &[]));
        assert!(!later.effective().settings.contains_key("build_user"));
    }

    /// What an executor refuses goes back to what was running before, with
    /// the executor's reason.
    #[test]
    fn an_executor_refusal_restores_the_previous_value() {
        let settings = WorkerSettings::from_env(vec![integer("AURCACHE_TEST_EXEC", "exec")]);
        let before = settings.with_snapshot(&snapshot("r1", &[("exec", "2")]));
        let candidate = before.with_snapshot(&snapshot("r2", &[("exec", "5")]));
        let refusals = BTreeMap::from([("exec".to_string(), "no cgroup".to_string())]);
        let after = candidate.refused(&refusals, &before);
        assert_eq!(after.raw("exec"), Some("2"));
        let report = &after.effective().settings["exec"];
        assert_eq!(report.status, SettingStatus::Rejected);
        assert_eq!(report.reason.as_deref(), Some("no cgroup"));
    }

    /// Every built-in default must be readable back as its own kind, since it
    /// is both what the worker runs and what the server is told the fallback
    /// is.
    #[test]
    fn built_in_defaults_are_valid_for_their_kind() {
        for spec in crate::settings::protocol_settings() {
            if let Some(written) = spec.default.as_written() {
                assert!(
                    spec.kind.validate(&written).is_ok(),
                    "{}'s default {written:?} is not a valid {:?}",
                    spec.key,
                    spec.kind
                );
            }
        }
    }
}
