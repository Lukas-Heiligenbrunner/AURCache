//! The worker's SSH key, which builds use and never hold.
//!
//! For each build, the worker loads the key into an OpenSSH `ssh-agent` of
//! its own ([`agent`]) and passes the build's logins to it over a link into
//! the build container, for as long as the build runs ([`link`]). The key
//! is never in the container. What a build can do is have the key sign, a
//! login or anything else, while it runs. See `container-workers.md`, "The
//! SSH link".
//!
//! The key is the worker's default key ([`ssh_key::DEFAULT`]), the one a
//! build is offered unless its package's `build_ssh_key` names another. A
//! container worker holds no named key ([`held`]), so a build whose package
//! names one is offered nothing, and its log says so, as on a chroot worker
//! that does not hold the key named.

pub mod agent;
pub mod link;

use agent::Agent;
use anyhow::{Context, Result, bail};
use aurcache_common::ssh_key;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The variable naming the SSH key's file, as on the chroot worker.
pub const KEY_VARIABLE: &str = "WORKER_GIT_SSH_KEY";
/// The variable naming the file of the hosts' keys, as on the chroot worker.
pub const HOSTS_VARIABLE: &str = "WORKER_SSH_KNOWN_HOSTS";

/// The files a worker reads its SSH key and the hosts' keys from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialFiles {
    pub key: PathBuf,
    pub known_hosts: PathBuf,
}

/// The key, in an ssh-agent for one build, and the hosts it is used with.
pub struct Credential {
    pub agent: Arc<Agent>,
    pub known_hosts: String,
}

impl CredentialFiles {
    /// The files [`KEY_VARIABLE`] and [`HOSTS_VARIABLE`] name, read through
    /// `var`.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        Self::from_settings(
            var(KEY_VARIABLE).map(PathBuf::from),
            var(HOSTS_VARIABLE).map(PathBuf::from),
            KEY_VARIABLE,
            HOSTS_VARIABLE,
        )
    }

    /// The files `key_variable` and `hosts_variable` name: none, or both.
    ///
    /// A key without host keys is refused: a container is new for every
    /// build, so an ssh left to trust the first key a host shows would trust
    /// any host, every time.
    pub fn from_settings(
        key: Option<PathBuf>,
        known_hosts: Option<PathBuf>,
        key_variable: &str,
        hosts_variable: &str,
    ) -> Result<Option<Self>> {
        match (key, known_hosts) {
            (None, None) => Ok(None),
            (Some(key), Some(known_hosts)) => Ok(Some(Self { key, known_hosts })),
            (Some(_), None) => bail!(
                "{key_variable} is set and {hosts_variable} is not: name a file with the host \
                 keys of the servers the key is used with, for example the output of \
                 `ssh-keyscan github.com`"
            ),
            (None, Some(_)) => bail!("{hosts_variable} is set and {key_variable} is not"),
        }
    }

    /// Whether both files are there to be read, without starting anything.
    /// Whether the key is one ssh-agent takes is [`Self::load`]'s to find.
    pub fn readable(&self) -> Result<()> {
        read_known_hosts(&self.known_hosts)?;
        std::fs::File::open(&self.key)
            .with_context(|| format!("reading the SSH key {}", self.key.display()))?;
        Ok(())
    }

    /// Read the hosts' keys, and start an ssh-agent that has the key.
    pub async fn load(&self) -> Result<Credential> {
        let known_hosts = read_known_hosts(&self.known_hosts)?;
        let agent = Agent::start(&self.key).await?;
        Ok(Credential {
            agent: Arc::new(agent),
            known_hosts,
        })
    }
}

/// The keys a container worker holds, by name, for
/// [`aurcache_worker_core::ssh_key::offer`]: its one key, when it has one, as
/// the default.
#[must_use]
pub fn held(files: Option<&CredentialFiles>) -> BTreeMap<String, CredentialFiles> {
    files
        .map(|files| (ssh_key::DEFAULT.to_string(), files.clone()))
        .into_iter()
        .collect()
}

fn read_known_hosts(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the SSH host keys {}", path.display()))?;
    let hosts = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .count();
    if hosts == 0 {
        bail!(
            "{} holds no host key: add those of the servers the key is used with, for \
             example the output of `ssh-keyscan github.com`",
            path.display()
        );
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_common::worker::{JobDescriptor, MirrorlistOffer};
    use aurcache_worker_core::ssh_key::{SshOffer, offer};

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
        }
    }

    /// The worker's key is offered to builds asking for the default key,
    /// and to no build whose package names another.
    #[test]
    fn the_key_is_offered_only_as_the_default() {
        let files = CredentialFiles {
            key: "/k".into(),
            known_hosts: "/h".into(),
        };
        let keys = held(Some(&files));
        for default in [None, Some("default")] {
            assert_eq!(offer(&job(default), &keys, true).key(), Some(&files));
        }
        let named = offer(&job(Some("gitlab")), &keys, true);
        assert_eq!(
            named,
            SshOffer::NotHeld {
                name: "gitlab".to_string()
            }
        );
        assert!(
            named
                .notice("hello")
                .unwrap()
                .contains("building without one")
        );

        let none = held(None);
        assert_eq!(offer(&job(None), &none, true), SshOffer::NoKey);
        assert!(matches!(
            offer(&job(Some("gitlab")), &none, true),
            SshOffer::NotHeld { .. }
        ));
    }

    #[test]
    fn a_key_comes_with_its_host_keys_or_not_at_all() {
        let named = |key: Option<&str>, hosts: Option<&str>| {
            CredentialFiles::from_settings(
                key.map(PathBuf::from),
                hosts.map(PathBuf::from),
                "KEY",
                "HOSTS",
            )
        };
        assert_eq!(named(None, None).unwrap(), None);
        assert_eq!(
            named(Some("/k"), Some("/h")).unwrap(),
            Some(CredentialFiles {
                key: "/k".into(),
                known_hosts: "/h".into()
            })
        );
        let said = format!("{:#}", named(Some("/k"), None).unwrap_err());
        assert!(
            said.contains("HOSTS") && said.contains("ssh-keyscan"),
            "{said}"
        );
        assert!(named(None, Some("/h")).is_err());
    }

    #[tokio::test]
    async fn both_files_are_read_and_checked() {
        let key = agent::tests::ed25519_key();
        let dir = tempfile::tempdir().unwrap();
        let files = CredentialFiles {
            key: dir.path().join("ssh-privatekey"),
            known_hosts: dir.path().join("known_hosts"),
        };
        std::fs::copy(&key.file, &files.key).unwrap();
        std::fs::write(&files.known_hosts, "# nothing\n\n").unwrap();
        let said = format!("{:#}", files.load().await.err().unwrap());
        assert!(said.contains("no host key"), "{said}");
        assert!(files.readable().is_err());
        std::fs::write(&files.known_hosts, "github.com ssh-ed25519 AAAA\n").unwrap();
        files.readable().unwrap();
        let loaded = files.load().await.unwrap();
        assert_eq!(loaded.known_hosts, "github.com ssh-ed25519 AAAA\n");
        assert_eq!(loaded.agent.identity(), key.listed);
        std::fs::write(&files.key, "not a key").unwrap();
        let said = format!("{:#}", files.load().await.err().unwrap());
        assert!(said.contains("ssh-privatekey"), "{said}");
        std::fs::remove_file(&files.key).unwrap();
        let said = format!("{:#}", files.readable().unwrap_err());
        assert!(said.contains("ssh-privatekey"), "{said}");
    }
}
