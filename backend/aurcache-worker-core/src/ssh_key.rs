//! Which of a worker's SSH keys a build is offered, and why none when it
//! asked for one.
//!
//! The server decides: a job names a key -- the package's `build_ssh_key`
//! setting -- or, by naming none, asks for [`ssh_key::DEFAULT`], the worker's
//! own. The worker offers the key by that name when it holds one, in
//! [`SshKeys`]: a lookup in the worker's own configuration, never anything
//! derived from the name. Decided here so every executor decides the same way
//! and says the same thing.
//!
//! An executor calls [`offer`] with the claimed job and acts on the
//! [`SshOffer`]: hand the build [`SshOffer::key`] when there is one, build
//! without a key when there is none, and write [`SshOffer::notice`] to its
//! log.

use aurcache_common::ssh_key;
use aurcache_common::worker::{JobDescriptor, SshKeyRequest};
use std::collections::BTreeMap;

/// The SSH keys a worker holds, by name.
///
/// `K` is whatever the executor hands a build to use one: an agent's socket,
/// a mounted secret. One per key, so a build handed one can use no other.
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

    /// Hold `key` as `name`, in place of any key held by that name already.
    pub fn insert(&mut self, name: &str, key: K) {
        self.by_name.insert(name.to_string(), key);
    }

    /// The key named `name`, if it is a name at all and this worker holds it.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&K> {
        ssh_key::validate_name(name).ok()?;
        self.by_name.get(name)
    }
}

/// What a build is offered of its worker's SSH keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshOffer<'k, K> {
    /// The key `name`, which the build may use.
    Offered { name: String, key: &'k K },
    /// The job names a key this worker does not hold. The build goes ahead
    /// without one: most never use it, and one that does fails at the fetch
    /// that needed it, with the notice above it in its log.
    NotHeld { name: String },
    /// No key, and nothing to say: the job asks for the default key on a
    /// worker that holds none, or (`{"name": null}`, which no server sends
    /// any more) for no key at all.
    NoKey,
}

impl<'k, K> SshOffer<'k, K> {
    /// The key to hand the build, if any.
    #[must_use]
    pub fn key(&self) -> Option<&'k K> {
        match self {
            Self::Offered { key, .. } => Some(key),
            Self::NotHeld { .. } | Self::NoKey => None,
        }
    }

    /// One line for `pkgbase`'s build log saying which key the build was
    /// offered, or that the key its package names is not here; `None` when
    /// there is nothing to say.
    #[must_use]
    pub fn notice(&self, pkgbase: &str) -> Option<String> {
        match self {
            Self::Offered { name, .. } => Some(format!("SSH key {name:?} offered to this build")),
            Self::NotHeld { name } => Some(format!(
                "this worker holds no SSH key named {name:?}, which the build_ssh_key \
                 setting names for {pkgbase}; building without one"
            )),
            Self::NoKey => None,
        }
    }
}

/// What `job`'s build is offered of `keys`.
#[must_use]
pub fn offer<'k, K>(job: &JobDescriptor, keys: &'k SshKeys<K>) -> SshOffer<'k, K> {
    match &job.ssh_key {
        // The default key, as every job asks for unless its package names
        // another; silent when this worker holds none.
        None => keys
            .get(ssh_key::DEFAULT)
            .map_or(SshOffer::NoKey, |key| SshOffer::Offered {
                name: ssh_key::DEFAULT.to_string(),
                key,
            }),
        Some(SshKeyRequest { name: None }) => SshOffer::NoKey,
        Some(SshKeyRequest { name: Some(name) }) => match keys.get(name) {
            Some(key) => SshOffer::Offered {
                name: name.clone(),
                key,
            },
            None => SshOffer::NotHeld { name: name.clone() },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_common::worker::MirrorlistOffer;

    fn job(ssh_key: Option<SshKeyRequest>) -> JobDescriptor {
        JobDescriptor {
            build_id: 1,
            pkgbase: "hello".to_string(),
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

    /// A worker holding one key, the default, behind an agent's socket.
    fn agent() -> SshKeys<&'static str> {
        SshKeys::with_default(Some("/agent.sock"))
    }

    fn offered(name: &str, key: &'static &'static str) -> SshOffer<'static, &'static str> {
        SshOffer::Offered {
            name: name.to_string(),
            key,
        }
    }

    /// A job naming nothing gets the default key, and its log says so.
    #[test]
    fn a_job_naming_nothing_gets_the_default_key() {
        let keys = agent();
        let offer = offer(&job(None), &keys);
        assert_eq!(offer, offered("default", &"/agent.sock"));
        assert_eq!(offer.key(), Some(&"/agent.sock"));
        assert_eq!(
            offer.notice("hello").unwrap(),
            "SSH key \"default\" offered to this build"
        );
    }

    /// A job naming the default outright gets it too.
    #[test]
    fn a_job_naming_the_default_gets_it() {
        let keys = agent();
        assert_eq!(
            offer(&job(Some(named("default"))), &keys),
            offered("default", &"/agent.sock")
        );
    }

    /// A job naming no key at all, as no server sends any more but protocol
    /// 1 still carries, is offered none, silently.
    #[test]
    fn a_job_naming_no_key_is_offered_none() {
        let keys = agent();
        let offer = offer(&job(Some(SshKeyRequest { name: None })), &keys);
        assert_eq!(offer, SshOffer::NoKey);
        assert_eq!(offer.notice("hello"), None);
    }

    /// A name the worker does not hold, or that is no name at all, is never
    /// looked up as anything else; the build goes ahead without a key, and
    /// its log says why.
    #[test]
    fn a_name_the_worker_does_not_hold_is_built_without() {
        let keys = agent();
        for name in ["gitlab", "../ssh/id_ed25519", "/run/secrets/key", "Default"] {
            let offer = offer(&job(Some(named(name))), &keys);
            assert_eq!(
                offer,
                SshOffer::NotHeld {
                    name: name.to_string()
                }
            );
            assert_eq!(offer.key(), None);
        }
        assert_eq!(
            offer(&job(Some(named("gitlab"))), &keys)
                .notice("hello")
                .unwrap(),
            "this worker holds no SSH key named \"gitlab\", which the build_ssh_key \
             setting names for hello; building without one"
        );
    }

    /// A worker with no key builds a job that names none without a word;
    /// one whose package names a key still hears that it is not here.
    #[test]
    fn a_worker_without_keys_is_silent_unless_a_key_is_named() {
        let keys = SshKeys::<&str>::default();
        let offer_default = offer(&job(None), &keys);
        assert_eq!(offer_default, SshOffer::NoKey);
        assert_eq!(offer_default.notice("hello"), None);
        assert!(matches!(
            offer(&job(Some(named("gitlab"))), &keys),
            SshOffer::NotHeld { .. }
        ));
    }
}
