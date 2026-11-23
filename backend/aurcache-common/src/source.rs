//! How a package's source is identified.
//!
//! Shared rather than defined beside the database entity: these shapes appear
//! in API requests and responses, so they have to be usable from a frontend
//! that has no database driver.

use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::str::FromStr;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GitSourceSpec {
    pub url: String,
    pub r#ref: String,
    pub subfolder: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
// Stored in one database column as its `Display` form. The derive that makes
// that work is a database concern, so it is gated: a frontend gets the same
// shape without a database driver.
#[cfg_attr(feature = "db", derive(sea_orm::DeriveValueType))]
#[cfg_attr(feature = "db", sea_orm(value_type = "String"))]
#[serde(tag = "type")]
pub enum SourceData {
    #[serde(rename = "aur")]
    Aur { name: String },
    #[serde(rename = "git")]
    Git {
        #[serde(flatten)]
        spec: GitSourceSpec,
    },
    #[serde(rename = "upload")]
    Upload { archive: Vec<u8> },
}

/// Whether a package someone typed is a git remote rather than an AUR name.
///
/// Git URLs either use the SCP-like `user@host:path` shorthand — any user, not
/// just `git`, e.g. `aur@aur.archlinux.org:foo.git` — or an explicit scheme
/// (`https://`, `ssh://`, `git://`, …). An AUR package name cannot contain `@`,
/// so anything with one is unambiguously a remote.
///
/// A bare `.git` suffix is deliberately not enough: AUR names legitimately end
/// in it (`paru-git`, `lab.git`), and with no scheme there is no filesystem to
/// resolve a scheme-less path against anyway. Those stay AUR names.
///
/// Shared because the CLI and the web UI both accept one field that is either
/// thing, and two copies of a guess like this would drift into disagreeing
/// about the same input.
#[must_use]
pub fn looks_like_git_url(s: &str) -> bool {
    s.contains('@') || s.contains("://")
}

/// Where an SSH git remote is: the account, the host, and the port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshRemote {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

impl SshRemote {
    /// How a `known_hosts` file names this host, as OpenSSH writes it: bare
    /// on the default port, `[host]:port` on any other.
    #[must_use]
    pub fn known_hosts_name(&self) -> String {
        let host = self.host.to_ascii_lowercase();
        match self.port {
            None | Some(22) => host,
            Some(port) => format!("[{host}]:{port}"),
        }
    }
}

/// How a git source is reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    Ssh(SshRemote),
    /// `https://`, `http://`, `git://`, `file://` or a local path: never SSH.
    Other,
}

/// The source URLs that the server reads, for the refusal of any other.
pub const SUPPORTED_GIT_URLS: &str = "a git source is an `https://`, `http://`, `git://` or \
     `file://` URL, `ssh://[user@]host[:port]/path`, `user@host:path`, or a local path \
     with no colon in it";

/// How `url` is reached, or `None` when it is not one of the
/// [`SUPPORTED_GIT_URLS`].
///
/// An allowlist, because libgit2 sends anything else with a colon in it over
/// SSH, and its certificate callback is told the host but not the port: the
/// host and port judged here must be the ones it connects to. Within these
/// forms, where the account and host admit no brackets, `%`, `@` or `:`,
/// libgit2 reads them the same way.
#[must_use]
pub fn transport(url: &str) -> Option<Transport> {
    const NOT_SSH: [&str; 4] = ["https://", "http://", "git://", "file://"];
    if NOT_SSH.iter().any(|scheme| url.starts_with(scheme)) || !url.contains(':') {
        return Some(Transport::Other);
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        let (authority, _path) = rest.split_once('/')?;
        let (user, host_port) = match authority.split_once('@') {
            Some((user, host_port)) => (Some(user), host_port),
            None => (None, authority),
        };
        let (host, port) = match host_port.split_once(':') {
            Some((host, port)) if port.bytes().all(|c| c.is_ascii_digit()) => {
                (host, Some(port.parse().ok()?))
            }
            Some(_) => return None,
            None => (host_port, None),
        };
        return ssh(user, host, port);
    }
    let (user, rest) = url.split_once('@')?;
    let (host, path) = rest.split_once(':')?;
    if path.is_empty() {
        return None;
    }
    ssh(Some(user), host, None)
}

/// The SSH remote that `url` names, if it is one.
///
/// Shared so that the add dialog says the server's key will be offered
/// exactly when the server would offer it.
#[must_use]
pub fn ssh_remote(url: &str) -> Option<SshRemote> {
    match transport(url)? {
        Transport::Ssh(remote) => Some(remote),
        Transport::Other => None,
    }
}

fn ssh(user: Option<&str>, host: &str, port: Option<u16>) -> Option<Transport> {
    let plain = |text: &str, extra: &[u8]| {
        !text.is_empty()
            && text
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c) || extra.contains(&c))
    };
    if !plain(host, b".") || user.is_some_and(|user| !plain(user, b".")) {
        return None;
    }
    Some(Transport::Ssh(SshRemote {
        user: user.map(str::to_string),
        host: host.to_string(),
        port,
    }))
}

/// How to write a source on screen: the AUR name, the git remote with its
/// ref and subfolder, or a placeholder for an upload.
///
/// The label is derived rather than stored: two sources with the same label
/// are the same source, which is what duplicate checks rely on — and a
/// failure is reported against what the request carried.
///
/// Shared for the same reason as [`looks_like_git_url`]: the add dialog, the
/// progress cards and the server's bulk-add report each had a copy, with only
/// punctuation drift between them.
#[must_use]
pub fn source_label(source: &SourceData) -> String {
    match source {
        SourceData::Aur { name } => name.clone(),
        SourceData::Git { spec } => {
            let mut label = spec.url.clone();
            if !spec.r#ref.is_empty() {
                label.push('#');
                label.push_str(&spec.r#ref);
            }
            if !spec.subfolder.is_empty() {
                label.push('/');
                label.push_str(&spec.subfolder);
            }
            label
        }
        // Never built from here — the upload it belongs to was never
        // implemented server-side — but the variant exists, so it gets a
        // label rather than a panic.
        SourceData::Upload { .. } => "uploaded archive".to_string(),
    }
}

impl From<GitSourceSpec> for SourceData {
    fn from(spec: GitSourceSpec) -> Self {
        Self::Git { spec }
    }
}

impl FromStr for SourceData {
    /// The error this actually produces, rather than `anyhow::Error` wrapping
    /// it. sea-orm 2.0 requires a `std::error::Error` here, and naming the real
    /// one is better than boxing it: `?` still converts it into `anyhow` at
    /// every call site that wants that.
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl SourceData {
    /// Unique cache key for this source.
    ///
    /// The git fields are length-prefixed, not plain-joined: they are URLs
    /// and refs, which contain `:`, so `url="a:b", ref="c"` and `url="a",
    /// ref="b:c"` would otherwise share a key — and check out over each
    /// other in the snapshot cache. Digits survive `sanitize_cache_key`,
    /// keeping the on-disk name unambiguous too.
    #[must_use]
    pub fn cache_key(&self) -> String {
        match self {
            Self::Aur { name } => format!("aur:{name}"),
            Self::Git { spec } => {
                format!(
                    "git:{}:{}:{}:{}:{}:{}",
                    spec.url.len(),
                    spec.url,
                    spec.r#ref.len(),
                    spec.r#ref,
                    spec.subfolder.len(),
                    spec.subfolder
                )
            }
            Self::Upload { .. } => String::from("upload"),
        }
    }
}

impl Display for SourceData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&serde_json::to_string(self).map_err(|_| std::fmt::Error)?)
    }
}

#[cfg(test)]
mod tests {
    use super::{SourceData, SshRemote, Transport, looks_like_git_url, ssh_remote, transport};

    #[test]
    fn the_supported_forms_are_read_and_nothing_else() {
        let ssh = |user: Option<&str>, host: &str, port| {
            Some(Transport::Ssh(SshRemote {
                user: user.map(str::to_string),
                host: host.to_string(),
                port,
            }))
        };
        for (url, expected) in [
            (
                "git@github.com:user/repo.git",
                ssh(Some("git"), "github.com", None),
            ),
            (
                "aur@aur.archlinux.org:paru",
                ssh(Some("aur"), "aur.archlinux.org", None),
            ),
            // A port is a path in the scp-like form, as git reads it.
            ("git@host:2222/repo", ssh(Some("git"), "host", None)),
            (
                "ssh://git@git.example.com:2222/srv/r.git",
                ssh(Some("git"), "git.example.com", Some(2222)),
            ),
            ("ssh://host/repo", ssh(None, "host", None)),
            ("https://github.com/user/repo.git", Some(Transport::Other)),
            ("git://host/repo", Some(Transport::Other)),
            ("file:///srv/git/repo.git", Some(Transport::Other)),
            ("/srv/git/repo.git", Some(Transport::Other)),
        ] {
            assert_eq!(transport(url), expected, "{url}");
        }
        // libgit2 would reach each of these over SSH, some on another port
        // than a plain reading gives.
        for url in [
            "[git@host:2222]:repo.git",
            "git@[::1]:repo",
            "ssh://[::1]/repo",
            "host:repo.git",
            "./a:b",
            "SSH://host/repo",
            "git+ssh://host/repo",
            "nope://host/repo",
            "ssh://deploy:hunter2@host/repo",
            "ssh://a@b@host/repo",
            "ssh://ho%73t/repo",
            "ssh://host:+22/repo",
            "ssh://host:/repo",
            "ssh://host",
            " git@host:repo",
            "git@host:",
        ] {
            assert_eq!(transport(url), None, "{url}");
        }
    }

    #[test]
    fn known_hosts_names_follow_openssh() {
        let name = |url| ssh_remote(url).unwrap().known_hosts_name();
        assert_eq!(name("git@GitHub.com:u/r"), "github.com");
        assert_eq!(name("ssh://git@github.com:22/u/r"), "github.com");
        assert_eq!(name("ssh://git@host:2222/r"), "[host]:2222");
    }

    #[test]
    fn detects_scp_like_git_urls() {
        assert!(looks_like_git_url("aur@aur.archlinux.org:paru"));
        assert!(looks_like_git_url("git@github.com:user/project"));
    }

    #[test]
    fn detects_scheme_git_urls() {
        assert!(looks_like_git_url("https://github.com/user/project"));
        assert!(looks_like_git_url("ssh://git@host/repo"));
        assert!(looks_like_git_url("git://host/repo"));
    }

    /// The suffix alone is not a URL: these are real AUR package names, and
    /// treating them as remotes would make them unaddable by name.
    #[test]
    fn does_not_treat_git_like_aur_names_as_urls() {
        assert!(!looks_like_git_url("paru-git"));
        assert!(!looks_like_git_url("lab.git"));
        assert!(!looks_like_git_url("hello"));
    }

    /// Colons move across field boundaries: `url="a:b", ref="c"` and
    /// `url="a", ref="b:c"` must not share a snapshot cache directory.
    #[test]
    fn git_cache_keys_survive_colons_in_every_field() {
        use crate::source::GitSourceSpec;
        let left = SourceData::Git {
            spec: GitSourceSpec {
                url: "a:b".to_string(),
                r#ref: "c".to_string(),
                subfolder: String::new(),
            },
        };
        let right = SourceData::Git {
            spec: GitSourceSpec {
                url: "a".to_string(),
                r#ref: "b:c".to_string(),
                subfolder: String::new(),
            },
        };
        assert_ne!(left.cache_key(), right.cache_key());
    }
}
