//! Which of a worker's SSH keys a build is offered, and why none when it
//! asked for one.
//!
//! The server decides: a job names a key -- the package's `build_ssh_key`
//! setting -- or, by naming none, asks for [`ssh_key::DEFAULT`], the worker's
//! own. The worker offers the key by that name when it holds one: a lookup in
//! the sockets of the agents it started, by key name, never anything derived
//! from the name.
//!
//! [`offer`] says what the claimed job's build is offered, as an
//! [`SshOffer`]: the build is handed [`SshOffer::key`] when there is one,
//! builds without a key when there is none, and has [`SshOffer::notice`]
//! written to its log.

use aurcache_common::ssh_key;
use aurcache_common::worker::JobDescriptor;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What a build is offered of its worker's SSH keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshOffer<'k> {
    /// The key `name`, whose agent's socket is `key`.
    Offered { name: String, key: &'k PathBuf },
    /// The job names a key this worker does not hold. The build goes ahead
    /// without one: most never use it, and one that does fails at the fetch
    /// that needed it, with the notice above it in its log.
    NotHeld { name: String },
    /// The job names a key this worker holds, but no longer offers: the
    /// sandbox stopped keeping builds from the named keys' agents, and the
    /// worker stopped them (see `agent::NamedKeysGate`). The build goes ahead
    /// without one, as for a key not held.
    Withheld { name: String },
    /// No key, and nothing to say: the job asks for the default key on a
    /// worker that holds none.
    NoKey,
}

impl<'k> SshOffer<'k> {
    /// The socket of the agent to hand the build, if any.
    #[must_use]
    pub fn key(&self) -> Option<&'k PathBuf> {
        match self {
            Self::Offered { key, .. } => Some(key),
            Self::NotHeld { .. } | Self::Withheld { .. } | Self::NoKey => None,
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
            Self::Withheld { name } => Some(format!(
                "SSH key {name:?} withheld: builds are no longer kept from its agent outside \
                 their chroot, so this worker has stopped it (the worker's log says why); \
                 building without one"
            )),
            Self::NoKey => None,
        }
    }
}

/// What `job`'s build is offered of `keys`, the sockets of the agents this
/// worker started, by key name: of the named ones, those other than
/// [`ssh_key::DEFAULT`], only while `named_keys_held`.
#[must_use]
pub fn offer<'k>(
    job: &JobDescriptor,
    keys: &'k BTreeMap<String, PathBuf>,
    named_keys_held: bool,
) -> SshOffer<'k> {
    match &job.ssh_key {
        // The default key, as every job asks for unless its package names
        // another; silent when this worker holds none.
        None => keys
            .get(ssh_key::DEFAULT)
            .map_or(SshOffer::NoKey, |key| SshOffer::Offered {
                name: ssh_key::DEFAULT.to_string(),
                key,
            }),
        Some(name) => match keys.get(name) {
            Some(_) if !named_keys_held && name != ssh_key::DEFAULT => {
                SshOffer::Withheld { name: name.clone() }
            }
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

    fn job(ssh_key: Option<&str>) -> JobDescriptor {
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
            ssh_key: ssh_key.map(str::to_string),
            build_hosts: None,
        }
    }

    /// A worker holding one key, the default, behind an agent's socket.
    fn agent() -> BTreeMap<String, PathBuf> {
        BTreeMap::from([(ssh_key::DEFAULT.to_string(), PathBuf::from("/agent.sock"))])
    }

    fn offered<'k>(name: &str, key: &'k PathBuf) -> SshOffer<'k> {
        SshOffer::Offered {
            name: name.to_string(),
            key,
        }
    }

    /// A job naming nothing gets the default key, and its log says so.
    #[test]
    fn a_job_naming_nothing_gets_the_default_key() {
        let keys = agent();
        let offer = offer(&job(None), &keys, true);
        assert_eq!(offer, offered("default", &keys["default"]));
        assert_eq!(offer.key(), Some(&PathBuf::from("/agent.sock")));
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
            offer(&job(Some("default")), &keys, true),
            offered("default", &keys["default"])
        );
    }

    /// A name the worker does not hold, or that is no name at all, is never
    /// looked up as anything else; the build goes ahead without a key, and
    /// its log says why.
    #[test]
    fn a_name_the_worker_does_not_hold_is_built_without() {
        let keys = agent();
        for name in ["gitlab", "../ssh/id_ed25519", "/run/secrets/key", "Default"] {
            let offer = offer(&job(Some(name)), &keys, true);
            assert_eq!(
                offer,
                SshOffer::NotHeld {
                    name: name.to_string()
                }
            );
            assert_eq!(offer.key(), None);
        }
        assert_eq!(
            offer(&job(Some("gitlab")), &keys, true)
                .notice("hello")
                .unwrap(),
            "this worker holds no SSH key named \"gitlab\", which the build_ssh_key \
             setting names for hello; building without one"
        );
    }

    /// Once the named keys are no longer held, a build whose package names
    /// one is offered nothing, and its log says why; the default key is
    /// offered as before, named outright or not.
    #[test]
    fn no_named_key_is_offered_once_they_are_not_held() {
        let mut keys = agent();
        keys.insert("gitlab".to_string(), PathBuf::from("/gitlab.sock"));
        assert_eq!(
            offer(&job(Some("gitlab")), &keys, true),
            offered("gitlab", &keys["gitlab"])
        );

        let withheld = offer(&job(Some("gitlab")), &keys, false);
        assert_eq!(
            withheld,
            SshOffer::Withheld {
                name: "gitlab".to_string()
            }
        );
        assert_eq!(withheld.key(), None);
        assert!(withheld.notice("hello").unwrap().contains("withheld"));
        for default in [None, Some("default")] {
            assert_eq!(
                offer(&job(default), &keys, false),
                offered("default", &keys["default"])
            );
        }
    }

    /// A worker with no key builds a job that names none without a word;
    /// one whose package names a key still hears that it is not here.
    #[test]
    fn a_worker_without_keys_is_silent_unless_a_key_is_named() {
        let keys = BTreeMap::new();
        let offer_default = offer(&job(None), &keys, true);
        assert_eq!(offer_default, SshOffer::NoKey);
        assert_eq!(offer_default.notice("hello"), None);
        assert!(matches!(
            offer(&job(Some("gitlab")), &keys, true),
            SshOffer::NotHeld { .. }
        ));
    }
}
