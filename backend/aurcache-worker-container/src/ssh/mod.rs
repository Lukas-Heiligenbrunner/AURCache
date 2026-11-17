//! The worker's SSH key, which builds use and never hold.
//!
//! The worker reads the key, holds it in memory, and answers the SSH agent
//! protocol for it ([`agent`]) over a link the build agent opens inside the
//! container for as long as it downloads the sources ([`link`]). See
//! `kubernetes-executor.md`, "Credentials and PGP keys".

pub mod agent;
pub mod key;
pub mod link;
mod wire;

use anyhow::{Context, Result, bail};
use key::SshKey;
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

/// The key, read, and the hosts it is used with.
pub struct Credential {
    pub key: Arc<SshKey>,
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

    /// Read both files.
    pub fn load(&self) -> Result<Credential> {
        let text = std::fs::read(&self.key)
            .with_context(|| format!("reading the SSH key {}", self.key.display()))?;
        let key = SshKey::parse(&text);
        drop(zeroize::Zeroizing::new(text));
        let key = key.with_context(|| format!("the SSH key {}", self.key.display()))?;
        let known_hosts = read_known_hosts(&self.known_hosts)?;
        Ok(Credential {
            key: Arc::new(key),
            known_hosts,
        })
    }
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

    #[test]
    fn both_files_are_read_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let files = CredentialFiles {
            key: dir.path().join("ssh-privatekey"),
            known_hosts: dir.path().join("known_hosts"),
        };
        std::fs::write(&files.key, key::tests::ed25519_key().private).unwrap();
        std::fs::write(&files.known_hosts, "# nothing\n\n").unwrap();
        let said = format!("{:#}", files.load().err().unwrap());
        assert!(said.contains("no host key"), "{said}");
        std::fs::write(&files.known_hosts, "github.com ssh-ed25519 AAAA\n").unwrap();
        let loaded = files.load().unwrap();
        assert_eq!(loaded.known_hosts, "github.com ssh-ed25519 AAAA\n");
        std::fs::write(&files.key, "not a key").unwrap();
        let said = format!("{:#}", files.load().err().unwrap());
        assert!(said.contains("ssh-privatekey"), "{said}");
    }
}
