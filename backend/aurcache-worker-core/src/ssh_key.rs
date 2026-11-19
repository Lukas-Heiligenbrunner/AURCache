//! Which of a worker's SSH keys a build is offered, and why not when none is.
//!
//! Three things decide it, all of them checked here so every executor decides
//! the same way and says the same thing:
//!
//! 1. the job names a key -- the package's `build_ssh_key` setting -- or, from
//!    a server that names none, implies [`ssh_key::DEFAULT`];
//! 2. this worker holds a key by that name, in [`SshKeys`]: a lookup in the
//!    worker's own configuration, never anything derived from the name;
//! 3. `WORKER_GIT_SSH_PACKAGES` ([`SshPackages`]), when set, lists the
//!    package: a ceiling the worker's operator sets and the server cannot
//!    widen, since it is plain environment the worker alone reads.
//!
//! An executor calls [`offer`] with the claimed job and acts on the
//! [`SshOffer`]: hand the build [`SshOffer::key`] when there is one, build
//! without a key when there is none, and write [`SshOffer::notice`] to its
//! log.

use aurcache_common::ssh_key;
use aurcache_common::worker::JobDescriptor;
use std::collections::BTreeMap;

/// The variable naming the packages a worker's SSH keys may go to.
pub const PACKAGES_VAR: &str = "WORKER_GIT_SSH_PACKAGES";

/// The packages this worker's SSH keys may be offered to:
/// `WORKER_GIT_SSH_PACKAGES`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SshPackages {
    /// Unset: every package the job allows.
    #[default]
    All,
    /// Exactly these pkgbases, matched as `WORKER_PACKAGES` matches: whole,
    /// case-sensitive names, no wildcards. Empty lists none.
    Only(Vec<String>),
}

impl SshPackages {
    /// Read `WORKER_GIT_SSH_PACKAGES` from this process's environment.
    ///
    /// Read here, by the worker, and never declared as a setting: a value the
    /// server could deliver would let it widen who gets the key.
    #[must_use]
    pub fn from_env() -> Self {
        let raw = std::env::var_os(PACKAGES_VAR);
        Self::parse(raw.as_deref().map(|raw| raw.to_string_lossy()).as_deref())
    }

    /// The list `raw` spells, in `WORKER_PACKAGES`' spelling (comma- or
    /// space-separated); `None` is unset.
    ///
    /// Set but empty lists no package rather than reading as unset: it is
    /// the only way to say "no build", and it fails closed.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            None => Self::All,
            Some(raw) => Self::Only(crate::config::parse_list(raw)),
        }
    }

    /// Whether this worker may offer a key to a build of `pkgbase`.
    #[must_use]
    pub fn offers(&self, pkgbase: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(names) => names.iter().any(|name| name == pkgbase),
        }
    }
}

/// The SSH keys a worker holds, by name.
///
/// `K` is whatever the executor hands a build to use one: an agent's socket,
/// a mounted secret. Only [`ssh_key::DEFAULT`] exists for now, but a lookup
/// by name is what lets a worker hold several without the job or the setting
/// changing shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshKeys<K> {
    by_name: BTreeMap<String, K>,
}

impl<K> Default for SshKeys<K> {
    fn default() -> Self {
        Self {
            by_name: BTreeMap::new(),
        }
    }
}

impl<K> SshKeys<K> {
    /// `key` as [`ssh_key::DEFAULT`], or no key at all.
    #[must_use]
    pub fn with_default(key: Option<K>) -> Self {
        let mut keys = Self::default();
        if let Some(key) = key {
            keys.by_name.insert(ssh_key::DEFAULT.to_string(), key);
        }
        keys
    }

    /// The key named `name`, if it is a name at all and this worker holds it.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&K> {
        ssh_key::validate_name(name).ok()?;
        self.by_name.get(name)
    }

    /// Whether this worker holds no key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// What a build is offered of its worker's SSH keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshOffer<'k, K> {
    /// The key `name`, which the build may use.
    Offered { name: String, key: &'k K },
    /// The job names no key.
    NotRequested,
    /// The job names a key this worker does not hold. The build goes ahead
    /// without one: most never use it, and one that does fails at the fetch
    /// that needed it, with the notice above it in its log.
    NotHeld { name: String },
    /// `WORKER_GIT_SSH_PACKAGES` does not list the package.
    NotAllowed,
    /// The job implies the default key, as a server that names none does,
    /// and this worker holds none: nothing to offer, and nothing to say.
    NoKeys,
}

impl<'k, K> SshOffer<'k, K> {
    /// The key to hand the build, if any.
    #[must_use]
    pub fn key(&self) -> Option<&'k K> {
        match self {
            Self::Offered { key, .. } => Some(key),
            _ => None,
        }
    }

    /// One line for `pkgbase`'s build log saying what the build was offered
    /// and, when nothing, why; `None` when there is nothing to say.
    #[must_use]
    pub fn notice(&self, pkgbase: &str) -> Option<String> {
        match self {
            Self::Offered { name, .. } => Some(format!("SSH key {name:?} offered to this build")),
            Self::NotRequested => Some(format!(
                "no SSH key offered: the build_ssh_key setting names none for {pkgbase}"
            )),
            Self::NotHeld { name } => Some(format!(
                "this worker holds no SSH key named {name:?}, which the build_ssh_key \
                 setting names for {pkgbase}; building without one"
            )),
            Self::NotAllowed => Some(format!(
                "no SSH key offered: {pkgbase} is not in this worker's {PACKAGES_VAR}"
            )),
            Self::NoKeys => None,
        }
    }
}

/// What `job`'s build is offered of `keys`, under `packages`.
#[must_use]
pub fn offer<'k, K>(
    job: &JobDescriptor,
    keys: &'k SshKeys<K>,
    packages: &SshPackages,
) -> SshOffer<'k, K> {
    let requested = match &job.ssh_key {
        // From a server that names no key: the default, as before there were
        // names, unless the worker holds none.
        None => None,
        Some(request) => match &request.name {
            None if keys.is_empty() => return SshOffer::NoKeys,
            None => return SshOffer::NotRequested,
            Some(name) => Some(name.as_str()),
        },
    };
    let name = requested.unwrap_or(ssh_key::DEFAULT);
    let Some(key) = keys.get(name) else {
        return match requested {
            Some(name) => SshOffer::NotHeld {
                name: name.to_string(),
            },
            None => SshOffer::NoKeys,
        };
    };
    if !packages.offers(&job.pkgbase) {
        return SshOffer::NotAllowed;
    }
    SshOffer::Offered {
        name: name.to_string(),
        key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_common::worker::{MirrorlistOffer, SshKeyRequest};

    fn job(pkgbase: &str, ssh_key: Option<SshKeyRequest>) -> JobDescriptor {
        JobDescriptor {
            build_id: 1,
            pkgbase: pkgbase.to_string(),
            arch: "x86_64".to_string(),
            build_flags: Vec::new(),
            persistent_builddir: false,
            makepkg_conf: String::new(),
            pacman_conf: String::new(),
            mirrorlist: MirrorlistOffer::Nothing,
            pgp_keys: Vec::new(),
            vcs_sources: Vec::new(),
            expected_packages: Vec::new(),
            timeout_secs: None,
            expected_version: "1-1".to_string(),
            ssh_key,
        }
    }

    fn named(name: &str) -> SshKeyRequest {
        SshKeyRequest {
            name: Some(name.to_string()),
        }
    }

    fn none() -> SshKeyRequest {
        SshKeyRequest { name: None }
    }

    /// A worker holding one key, the default, behind an agent's socket.
    fn agent() -> &'static SshKeys<&'static str> {
        static KEYS: std::sync::LazyLock<SshKeys<&'static str>> =
            std::sync::LazyLock::new(|| SshKeys::with_default(Some("/agent.sock")));
        &KEYS
    }

    fn offered(name: &str) -> SshOffer<'static, &'static str> {
        SshOffer::Offered {
            name: name.to_string(),
            key: &"/agent.sock",
        }
    }

    #[test]
    fn unset_is_every_package() {
        let packages = SshPackages::parse(None);
        assert_eq!(packages, SshPackages::All);
        assert!(packages.offers("anything"));
    }

    /// Set but empty is "none", not unset: it fails closed.
    #[test]
    fn empty_lists_no_package() {
        for raw in ["", " ", ",", " , "] {
            let packages = SshPackages::parse(Some(raw));
            assert_eq!(packages, SshPackages::Only(Vec::new()), "{raw:?}");
            assert!(!packages.offers("unreal-engine"), "{raw:?}");
            assert!(!packages.offers(""), "{raw:?}");
        }
    }

    /// The spelling `WORKER_PACKAGES` takes, so the two can hold the same text.
    #[test]
    fn the_list_is_comma_or_space_separated() {
        assert_eq!(
            SshPackages::parse(Some("unreal-engine, epic-tools other")),
            SshPackages::Only(vec![
                "unreal-engine".to_string(),
                "epic-tools".to_string(),
                "other".to_string()
            ])
        );
    }

    /// Whole, case-sensitive names: no prefix, no wildcard, no case folding.
    #[test]
    fn membership_is_exact() {
        let packages = SshPackages::parse(Some("unreal-engine"));
        assert!(packages.offers("unreal-engine"));
        assert!(!packages.offers("Unreal-Engine"));
        assert!(!packages.offers("unreal"));
        assert!(!packages.offers("unreal-engine-git"));
        assert!(!SshPackages::parse(Some("unreal-*")).offers("unreal-engine"));
    }

    /// A job from a server that names no key gets the default, as before,
    /// still under the worker's ceiling.
    #[test]
    fn a_job_naming_nothing_gets_the_default_key() {
        assert_eq!(
            offer(&job("hello", None), agent(), &SshPackages::All),
            offered("default")
        );
        let ceiling = SshPackages::parse(Some("unreal-engine"));
        assert_eq!(
            offer(&job("hello", None), agent(), &ceiling),
            SshOffer::NotAllowed
        );
        assert_eq!(
            offer(&job("unreal-engine", None), agent(), &ceiling),
            offered("default")
        );
    }

    #[test]
    fn a_named_key_held_and_allowed_is_offered() {
        let ceiling = SshPackages::parse(Some("unreal-engine"));
        let offer = offer(
            &job("unreal-engine", Some(named("default"))),
            agent(),
            &ceiling,
        );
        assert_eq!(offer.key(), Some(&"/agent.sock"));
        assert_eq!(offer, offered("default"));
    }

    #[test]
    fn a_job_naming_no_key_is_offered_none() {
        let offer = offer(&job("hello", Some(none())), agent(), &SshPackages::All);
        assert_eq!(offer, SshOffer::NotRequested);
        assert_eq!(offer.key(), None);
    }

    /// The ceiling is checked after the job: a job that asks for a key on a
    /// worker whose list leaves the package out is refused for that reason.
    #[test]
    fn the_ceiling_refuses_an_unlisted_package() {
        let ceiling = SshPackages::parse(Some("unreal-engine"));
        let offer = offer(&job("hello", Some(named("default"))), agent(), &ceiling);
        assert_eq!(offer, SshOffer::NotAllowed);
        assert_eq!(offer.key(), None);
        assert_eq!(
            offer.notice("hello").unwrap(),
            "no SSH key offered: hello is not in this worker's WORKER_GIT_SSH_PACKAGES"
        );
    }

    /// A name the worker does not hold, or that is no name at all, is never
    /// looked up as anything else, and the build is handed no key.
    #[test]
    fn a_name_the_worker_does_not_hold_is_not_held() {
        for name in ["gitlab", "../ssh/id_ed25519", "/run/secrets/key", "Default"] {
            let offer = offer(&job("hello", Some(named(name))), agent(), &SshPackages::All);
            assert_eq!(
                offer,
                SshOffer::NotHeld {
                    name: name.to_string()
                }
            );
            assert_eq!(offer.key(), None);
        }
        let no_keys = SshKeys::<&str>::default();
        let offer = offer(
            &job("hello", Some(named("default"))),
            &no_keys,
            &SshPackages::All,
        );
        assert!(matches!(offer, SshOffer::NotHeld { .. }), "{offer:?}");
        assert_eq!(offer.key(), None);
    }

    /// A build whose setting names a key its worker does not hold goes ahead
    /// without one, and its log says so: a package that never fetches over
    /// SSH builds, and one that does fails at that fetch with the reason
    /// above it.
    #[test]
    fn a_key_the_worker_does_not_hold_is_built_without() {
        let offer = offer(
            &job("hello", Some(named("gitlab"))),
            agent(),
            &SshPackages::All,
        );
        assert_eq!(offer.key(), None);
        assert_eq!(
            offer.notice("hello").unwrap(),
            "this worker holds no SSH key named \"gitlab\", which the build_ssh_key \
             setting names for hello; building without one"
        );
    }

    /// A worker with no key says nothing about keys to a build that did not
    /// insist on one: not about the job, not about the ceiling.
    #[test]
    fn a_worker_without_keys_is_silent() {
        let keys = SshKeys::<&str>::default();
        let ceiling = SshPackages::parse(Some("other"));
        for request in [None, Some(none())] {
            let offer = offer(&job("hello", request), &keys, &ceiling);
            assert_eq!(offer, SshOffer::NoKeys);
            assert_eq!(offer.notice("hello"), None);
        }
    }

    /// Each refusal says its own reason.
    #[test]
    fn every_refusal_explains_itself() {
        let notices: Vec<_> = [
            SshOffer::<&str>::NotRequested,
            SshOffer::NotHeld {
                name: "gitlab".to_string(),
            },
            SshOffer::NotAllowed,
        ]
        .iter()
        .map(|offer| offer.notice("hello").unwrap())
        .collect();
        assert!(notices[0].contains("build_ssh_key"), "{}", notices[0]);
        assert!(notices[1].contains("\"gitlab\""), "{}", notices[1]);
        assert!(notices[2].contains(PACKAGES_VAR), "{}", notices[2]);
    }
}
