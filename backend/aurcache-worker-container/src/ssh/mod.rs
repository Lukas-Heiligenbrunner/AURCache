//! The worker's SSH key, which builds use through an ssh-agent and never
//! hold. See "The SSH link" in `design/in-progress/container-workers.md`.

pub mod agent;
pub mod link;

use agent::Agent;
use anyhow::{Context, Result, bail};
use aurcache_common::ssh_key;
use std::collections::BTreeMap;
use std::path::PathBuf;

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
    pub agent: Agent,
    pub known_hosts: String,
}

impl CredentialFiles {
    /// The files that [`KEY_VARIABLE`] and [`HOSTS_VARIABLE`] name, read
    /// through `var`: none, or both.
    ///
    /// A key without host keys is refused: a container is new for every
    /// build, so an ssh left to trust the first key that a host shows would
    /// trust any host, every time.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        match (var(KEY_VARIABLE), var(HOSTS_VARIABLE)) {
            (None, None) => Ok(None),
            (Some(key), Some(known_hosts)) => Ok(Some(Self {
                key: key.into(),
                known_hosts: known_hosts.into(),
            })),
            (Some(_), None) => bail!(
                "{KEY_VARIABLE} is set and {HOSTS_VARIABLE} is not: name a file with the host \
                 keys of the servers that the key is used with, for example the output of \
                 `ssh-keyscan github.com`"
            ),
            (None, Some(_)) => bail!("{HOSTS_VARIABLE} is set and {KEY_VARIABLE} is not"),
        }
    }

    /// Read the hosts' keys, and start an ssh-agent that has the key.
    pub async fn load(&self) -> Result<Credential> {
        let known_hosts = std::fs::read_to_string(&self.known_hosts)
            .with_context(|| format!("reading the SSH host keys {}", self.known_hosts.display()))?;
        let agent = Agent::start(&self.key).await?;
        Ok(Credential { agent, known_hosts })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_held_as_the_default() {
        let files = CredentialFiles {
            key: "/k".into(),
            known_hosts: "/h".into(),
        };
        assert_eq!(
            (held(Some(&files)), held(None)),
            (
                BTreeMap::from([(ssh_key::DEFAULT.to_string(), files)]),
                BTreeMap::new()
            )
        );
    }

    /// Host keys that cannot be read fail with the file's name.
    #[tokio::test]
    async fn host_keys_that_cannot_be_read_name_the_file() {
        let files = CredentialFiles {
            key: "/nonexistent/key".into(),
            known_hosts: "/nonexistent/known_hosts".into(),
        };
        let Err(error) = files.load().await else {
            panic!("loaded host keys that are not there");
        };
        assert!(
            format!("{error:#}").contains("/nonexistent/known_hosts"),
            "{error:#}"
        );
    }

    #[test]
    fn a_key_comes_with_its_host_keys_or_not_at_all() {
        let named = |key: Option<&str>, hosts: Option<&str>| {
            CredentialFiles::from_vars(|name| match name {
                KEY_VARIABLE => key.map(str::to_string),
                HOSTS_VARIABLE => hosts.map(str::to_string),
                _ => None,
            })
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
            said.contains(HOSTS_VARIABLE) && said.contains("ssh-keyscan"),
            "{said}"
        );
        assert!(named(None, Some("/h")).is_err());
    }

    #[tokio::test]
    async fn both_files_are_read() {
        let key = agent::tests::ed25519_key();
        let dir = tempfile::tempdir().unwrap();
        let files = CredentialFiles {
            key: dir.path().join("ssh-privatekey"),
            known_hosts: dir.path().join("known_hosts"),
        };
        std::fs::copy(&key.file, &files.key).unwrap();
        std::fs::write(&files.known_hosts, "github.com ssh-ed25519 AAAA\n").unwrap();
        let loaded = files.load().await.unwrap();
        assert_eq!(loaded.known_hosts, "github.com ssh-ed25519 AAAA\n");
        assert_eq!(agent::tests::listed(loaded.agent.socket()), key.listed);
        std::fs::write(&files.key, "not a key").unwrap();
        let said = format!("{:#}", files.load().await.err().unwrap());
        assert!(said.contains("ssh-privatekey"), "{said}");
    }
}
