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
    /// The account the URL names (`git` in `git@github.com:user/repo`), if
    /// it names one.
    pub user: Option<String>,
    /// The host, without the brackets an IPv6 literal is written in.
    pub host: String,
    /// The port, when the URL gives one.
    pub port: Option<u16>,
    /// The repository on that host, as the URL writes it: absolute after an
    /// `ssh://` authority (`/srv/repo.git`), relative to the account's home in
    /// the scp-like form (`user/repo.git`). Empty when the URL names none.
    pub path: String,
}

impl SshRemote {
    /// How a `known_hosts` file names this host: lower case, bare on the
    /// default port, `[host]:port` on any other, as OpenSSH writes it -- and
    /// hashes it, so a hashed entry for `github.com` matches `GitHub.com`.
    #[must_use]
    pub fn known_hosts_name(&self) -> String {
        let host = self.host.to_ascii_lowercase();
        match self.port {
            None | Some(22) => host,
            Some(port) => format!("[{host}]:{port}"),
        }
    }
}

/// The SSH remote `url` names, or `None` when it is reached some other way.
///
/// The forms git itself reads as SSH: a URL with an `ssh://` scheme (or
/// `git+ssh://`, `ssh+git://`), and the scp-like `[user@]host:path`. As for
/// git, the scp-like form needs a colon before any slash: `./a:b` and
/// `/srv/a:b` are local paths, not hosts called `.` and `/srv/a`.
///
/// Shared because the server and the add dialog ask the same question of
/// the same string -- the server to know whether to offer its key and check
/// a host key, the dialog to say which key that will be -- and two answers
/// would drift apart.
#[must_use]
pub fn ssh_remote(url: &str) -> Option<SshRemote> {
    let url = url.trim();
    match url.split_once("://") {
        Some((scheme, rest)) => {
            // Case-sensitive, as libgit2 compares it: `SSH://` is not a URL
            // it would reach over SSH.
            if !matches!(scheme, "ssh" | "git+ssh" | "ssh+git") {
                return None;
            }
            let (authority, path) = match rest.find('/') {
                Some(slash) => rest.split_at(slash),
                None => (rest, ""),
            };
            let (user, host_port) = match authority.rsplit_once('@') {
                // A password, if the URL carries one, is not the account.
                Some((userinfo, host)) => (userinfo.split(':').next(), host),
                None => (None, authority),
            };
            let HostPort { host, port } = split_host_port(host_port)?;
            remote(user, host, port, path)
        }
        None => {
            // `user@` ends at the first `@`, and only when nothing before it
            // is part of a path or a port.
            let (user, rest) = match url.find('@') {
                Some(at) if !url[..at].contains(['/', ':']) => (Some(&url[..at]), &url[at + 1..]),
                _ => (None, url),
            };
            let (host, path) = if let Some(bracketed) = rest.strip_prefix('[') {
                let (inside, after) = bracketed.split_once(']')?;
                (inside, after.strip_prefix(':')?)
            } else {
                let (host, path) = rest.split_once(':')?;
                if host.contains('/') {
                    return None;
                }
                (host, path)
            };
            remote(user, host, None, path)
        }
    }
}

/// A URL authority's host, and its port if it gives one.
struct HostPort<'a> {
    host: &'a str,
    port: Option<u16>,
}

/// `host[:port]` or `[v6-literal][:port]`, or `None` when the port is not one.
fn split_host_port(authority: &str) -> Option<HostPort<'_>> {
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (inside, after) = bracketed.split_once(']')?;
        match after.strip_prefix(':') {
            Some(port) => (inside, Some(port)),
            None if after.is_empty() => (inside, None),
            None => return None,
        }
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port = match port {
        // `host:` is git's way of writing the default port.
        None | Some("") => None,
        Some(port) => Some(port.parse().ok()?),
    };
    Some(HostPort { host, port })
}

fn remote(user: Option<&str>, host: &str, port: Option<u16>, path: &str) -> Option<SshRemote> {
    if host.is_empty() {
        return None;
    }
    Some(SshRemote {
        user: user.filter(|u| !u.is_empty()).map(str::to_string),
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
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
    use super::{SourceData, SshRemote, looks_like_git_url, ssh_remote};

    fn ssh(user: Option<&str>, host: &str, port: Option<u16>, path: &str) -> SshRemote {
        SshRemote {
            user: user.map(str::to_string),
            host: host.to_string(),
            port,
            path: path.to_string(),
        }
    }

    #[test]
    fn ssh_scheme_urls_are_ssh_remotes() {
        assert_eq!(
            ssh_remote("ssh://git@github.com/user/repo.git"),
            Some(ssh(Some("git"), "github.com", None, "/user/repo.git"))
        );
        assert_eq!(
            ssh_remote("ssh://git@git.example.com:2222/srv/repo.git"),
            Some(ssh(
                Some("git"),
                "git.example.com",
                Some(2222),
                "/srv/repo.git"
            ))
        );
        // makepkg's spelling, and the reverse one git also accepts.
        assert_eq!(
            ssh_remote("git+ssh://aur@aur.archlinux.org/paru.git"),
            Some(ssh(Some("aur"), "aur.archlinux.org", None, "/paru.git"))
        );
        assert_eq!(
            ssh_remote("ssh+git://host/repo"),
            Some(ssh(None, "host", None, "/repo"))
        );
        assert_eq!(
            ssh_remote("ssh://git@[2001:db8::1]:2222/repo"),
            Some(ssh(Some("git"), "2001:db8::1", Some(2222), "/repo"))
        );
        assert_eq!(
            ssh_remote("ssh://[::1]/repo"),
            Some(ssh(None, "::1", None, "/repo"))
        );
        // The account, not the password beside it.
        assert_eq!(
            ssh_remote("ssh://deploy:hunter2@host/repo"),
            Some(ssh(Some("deploy"), "host", None, "/repo"))
        );
        assert_eq!(
            ssh_remote("ssh://host:/repo"),
            Some(ssh(None, "host", None, "/repo"))
        );
    }

    #[test]
    fn scp_like_remotes_are_ssh_remotes() {
        assert_eq!(
            ssh_remote("git@github.com:user/repo.git"),
            Some(ssh(Some("git"), "github.com", None, "user/repo.git"))
        );
        assert_eq!(
            ssh_remote("aur@aur.archlinux.org:paru"),
            Some(ssh(Some("aur"), "aur.archlinux.org", None, "paru"))
        );
        assert_eq!(
            ssh_remote("host:repo.git"),
            Some(ssh(None, "host", None, "repo.git"))
        );
        assert_eq!(
            ssh_remote("git@[::1]:repo.git"),
            Some(ssh(Some("git"), "::1", None, "repo.git"))
        );
        // Pasted with the line it came from.
        assert_eq!(
            ssh_remote("  git@gitlab.com:group/sub/repo.git\n"),
            Some(ssh(Some("git"), "gitlab.com", None, "group/sub/repo.git"))
        );
    }

    /// The path is what tells two repositories on one host apart, which is
    /// what choosing a key per repository would need.
    #[test]
    fn the_repository_path_is_kept_as_written() {
        let path = |url| ssh_remote(url).unwrap().path;
        assert_eq!(
            path("ssh://git@host:2222/srv/git/pkg.git"),
            "/srv/git/pkg.git"
        );
        assert_eq!(path("git@github.com:org/repo.git"), "org/repo.git");
        assert_eq!(path("ssh://host/~user/repo"), "/~user/repo");
        assert_eq!(path("git@[::1]:/abs/repo"), "/abs/repo");
        assert_eq!(path("ssh://host"), "");
        assert_eq!(path("host:"), "");
    }

    #[test]
    fn other_remotes_are_not_ssh_remotes() {
        for url in [
            "https://github.com/user/repo.git",
            "http://user@host:8080/repo.git",
            "git://host/repo",
            "file:///srv/git/repo.git",
            "/srv/git/repo.git",
            // A colon after a slash is part of a path, as git reads it.
            "./a:b",
            "/srv/a:b",
            "dir/a:b",
            "paru-git",
            // Not a port: refused rather than guessed at.
            "ssh://host:ssh/repo",
            // libgit2 reads the scheme case-sensitively, so neither does this.
            "SSH://host/repo",
            "ssh:///repo",
            "@:repo",
        ] {
            assert_eq!(ssh_remote(url), None, "{url}");
        }
    }

    #[test]
    fn known_hosts_names_follow_openssh() {
        let name = |url| ssh_remote(url).unwrap().known_hosts_name();
        assert_eq!(name("git@github.com:u/r"), "github.com");
        assert_eq!(name("ssh://git@github.com:22/u/r"), "github.com");
        assert_eq!(name("ssh://git@host:2222/r"), "[host]:2222");
        assert_eq!(name("ssh://[::1]:2222/r"), "[::1]:2222");
        assert_eq!(name("git@GitHub.com:u/r"), "github.com");
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
