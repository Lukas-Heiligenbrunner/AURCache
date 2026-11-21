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

/// How libgit2 reaches a remote, as far as SSH is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Over SSH, to this host and port.
    Ssh(SshRemote),
    /// Over SSH, to no host that can be named as libgit2 will connect to it:
    /// a "host" with a slash in it, a port that is not a number, a scheme
    /// libgit2 has no transport for. Nothing can check that host's key.
    UnreadableSsh,
    /// Over SSH, in git's bracketed scp-like form (`[user@host:port]:path`),
    /// which is not read: written as `ssh://user@host:port/path` instead, it
    /// names the same remote.
    BracketedScp,
    /// Not over SSH: `https://`, `git://`, `file://`, a local path.
    Other,
}

/// The schemes libgit2 reaches over SSH.
const SSH_SCHEMES: [&str; 3] = ["ssh://", "ssh+git://", "git+ssh://"];
/// libgit2's other transports.
const OTHER_SCHEMES: [&str; 4] = ["git://", "http://", "https://", "file://"];

/// How libgit2 reaches `url`, decided as libgit2 1.9 decides it.
///
/// It picks a transport by the URL's prefix, ignoring case (`SSH://` is
/// SSH), and sends anything else with a colon in it over SSH: the scp-like
/// `[user@]host:path`, but also `./a:b`, which git would take for a local
/// path, and a scheme it has no transport for. What it sends over SSH is
/// [`Transport::Ssh`] when the host and port it connects to can be named --
/// read as it reads them -- [`Transport::BracketedScp`] in the bracketed
/// scp-like form, which is not read, and [`Transport::UnreadableSsh`]
/// otherwise.
///
/// `url` is read exactly as given, untrimmed: libgit2 reads a leading space
/// as part of the account or the host, and a host named here from a trimmed
/// copy could be judged on another port than the one connected to.
#[must_use]
pub fn transport(url: &str) -> Transport {
    let ssh = |remote: Option<SshRemote>| remote.map_or(Transport::UnreadableSsh, Transport::Ssh);
    if let Some(rest) = SSH_SCHEMES
        .iter()
        .find_map(|scheme| strip_prefix_ignoring_case(url, scheme))
    {
        return ssh(url_remote(rest));
    }
    let other = OTHER_SCHEMES
        .iter()
        .any(|scheme| strip_prefix_ignoring_case(url, scheme).is_some());
    if other || !url.contains(':') {
        return Transport::Other;
    }
    if has_scheme(url) {
        return Transport::UnreadableSsh;
    }
    scp_transport(url)
}

/// The SSH remote `url` names, or `None` when libgit2 reaches it some other
/// way or to a host that cannot be named; see [`transport`].
///
/// Shared because the server and the add dialog ask the same question of
/// the same string -- the server to know whether to offer its key and check
/// a host key, the dialog to say which key that will be -- and two answers
/// would drift apart.
#[must_use]
pub fn ssh_remote(url: &str) -> Option<SshRemote> {
    match transport(url) {
        Transport::Ssh(remote) => Some(remote),
        Transport::UnreadableSsh | Transport::BracketedScp | Transport::Other => None,
    }
}

/// `text` without `prefix`, compared ignoring ASCII case.
fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Whether `url` starts with a scheme and `://`, as libgit2's
/// `git_net_str_is_url` decides it.
fn has_scheme(url: &str) -> bool {
    let scheme = url
        .bytes()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
        .count();
    url[scheme..].starts_with("://")
}

/// What follows an SSH scheme's `://`: `[user[:password]@]host[:port][/path]`.
///
/// libgit2 reads the authority from its end -- the port after the last
/// colon, the host after the last `@`, the account before the password's
/// colon -- and so is it read here. A host it would percent-decode is not
/// named: the name checked would not be the one connected to.
fn url_remote(rest: &str) -> Option<SshRemote> {
    let authority = rest
        .split_once('/')
        .map_or(rest, |(authority, _)| authority);
    let (user, host_port) = match authority.rsplit_once('@') {
        // A password, if the URL carries one, is not the account.
        Some((userinfo, host)) => (
            Some(userinfo.rsplit_once(':').map_or(userinfo, |(user, _)| user)),
            host,
        ),
        None => (None, authority),
    };
    let HostPort { host, port } = split_host_port(host_port)?;
    if host.contains('%') {
        return None;
    }
    remote(user, host, port)
}

/// git's scp-like form, `[user@]host:path`.
///
/// libgit2 reads the account up to the first `@` that comes before the
/// first colon, and the host up to that colon. Its parser also takes
/// brackets around the account and the host, inside which a port may follow
/// the host (`[git@host:2222]:path`), but that is a state machine whose
/// every corner would have to be mirrored for the port judged here to be the
/// one connected to -- and around an IPv6 literal (`git@[::1]:path`) it keeps
/// the brackets in the host, which then fails to resolve. Any bracket in the
/// account or the host is [`Transport::BracketedScp`] instead, and refused;
/// `ssh://git@[::1]/path` reaches an IPv6 literal.
fn scp_transport(given: &str) -> Transport {
    // libgit2 refuses a URL that starts with either.
    if given.starts_with(['@', ':']) {
        return Transport::UnreadableSsh;
    }
    let (user, rest) = match given.split_once('@') {
        Some((user, rest)) if !user.contains(':') => (Some(user), rest),
        _ => (None, given),
    };
    let Some((host, path)) = rest.split_once(':') else {
        return Transport::UnreadableSsh;
    };
    let bracketed = |text: &str| text.contains(['[', ']']);
    if user.is_some_and(bracketed) || bracketed(host) {
        return Transport::BracketedScp;
    }
    // Nothing after the colon is no path, which libgit2 refuses; a "host"
    // with a slash in it is a local path.
    if path.is_empty() || host.contains('/') {
        return Transport::UnreadableSsh;
    }
    remote(user, host, None).map_or(Transport::UnreadableSsh, Transport::Ssh)
}

/// A port in decimal digits and nothing else -- no sign, no space, no service
/// name, each of which libgit2 or the resolver could read another way.
fn parse_port(port: &str) -> Option<u16> {
    if !port.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    port.parse().ok()
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
        Some(port) => Some(parse_port(port)?),
    };
    Some(HostPort { host, port })
}

fn remote(user: Option<&str>, host: &str, port: Option<u16>) -> Option<SshRemote> {
    if host.is_empty() {
        return None;
    }
    Some(SshRemote {
        user: user.filter(|u| !u.is_empty()).map(str::to_string),
        host: host.to_string(),
        port,
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
    use super::{SourceData, SshRemote, Transport, looks_like_git_url, ssh_remote, transport};

    fn ssh(user: Option<&str>, host: &str, port: Option<u16>) -> SshRemote {
        SshRemote {
            user: user.map(str::to_string),
            host: host.to_string(),
            port,
        }
    }

    #[test]
    fn ssh_scheme_urls_are_ssh_remotes() {
        assert_eq!(
            ssh_remote("ssh://git@github.com/user/repo.git"),
            Some(ssh(Some("git"), "github.com", None))
        );
        assert_eq!(
            ssh_remote("ssh://git@git.example.com:2222/srv/repo.git"),
            Some(ssh(Some("git"), "git.example.com", Some(2222)))
        );
        // makepkg's spelling, and the reverse one git also accepts.
        assert_eq!(
            ssh_remote("git+ssh://aur@aur.archlinux.org/paru.git"),
            Some(ssh(Some("aur"), "aur.archlinux.org", None))
        );
        // libgit2 picks the transport ignoring the scheme's case.
        assert_eq!(
            ssh_remote("SSH://git@Host/repo"),
            Some(ssh(Some("git"), "Host", None))
        );
        assert_eq!(
            ssh_remote("Git+SSH://host:2222/repo"),
            Some(ssh(None, "host", Some(2222)))
        );
        assert_eq!(
            ssh_remote("ssh+git://host/repo"),
            Some(ssh(None, "host", None))
        );
        assert_eq!(
            ssh_remote("ssh://git@[2001:db8::1]:2222/repo"),
            Some(ssh(Some("git"), "2001:db8::1", Some(2222)))
        );
        assert_eq!(ssh_remote("ssh://[::1]/repo"), Some(ssh(None, "::1", None)));
        // The account, not the password beside it; the password starts at
        // the last colon, as libgit2 reads it.
        assert_eq!(
            ssh_remote("ssh://deploy:hunter2@host/repo"),
            Some(ssh(Some("deploy"), "host", None))
        );
        assert_eq!(
            ssh_remote("ssh://a:b:c@host/repo"),
            Some(ssh(Some("a:b"), "host", None))
        );
        assert_eq!(
            ssh_remote("ssh://host:/repo"),
            Some(ssh(None, "host", None))
        );
    }

    /// git's brackets around the account and host may hold a port, which
    /// libgit2 connects to, and around an IPv6 literal stay in the host
    /// libgit2 resolves. That form is refused, whatever is inside, rather
    /// than read: `ssh://` says the same.
    #[test]
    fn the_bracketed_scp_form_is_refused() {
        for url in [
            "[git@host:2222]:repo.git",
            "[host:2222]:repo.git",
            "[git@host]:repo.git",
            "[host]:repo.git",
            "[[::1]:2222]:repo.git",
            "[git@host:2222]",
            "[git@host:2222]x:repo",
            "[host]x:repo",
            "host]:repo",
            "[[[host]:repo",
            "[host:ssh]:repo",
            "git@[host:2222]:repo",
            "git@[host]:repo",
            "[::1]x:repo",
            "git@[::1]:repo.git",
            "[2001:db8::1]:repo.git",
            // A leading space is part of the account to libgit2, which then
            // connects to port 22; refused all the same.
            " [git@host:2222]:repo.git",
        ] {
            assert_eq!(transport(url), Transport::BracketedScp, "{url}");
        }
    }

    /// Read untrimmed, as libgit2 reads it: leading space is part of the
    /// account, trailing space part of the path.
    #[test]
    fn a_url_is_read_as_given() {
        assert_eq!(
            ssh_remote("  git@gitlab.com:group/repo.git\n"),
            Some(ssh(Some("  git"), "gitlab.com", None))
        );
    }

    #[test]
    fn scp_like_remotes_are_ssh_remotes() {
        assert_eq!(
            ssh_remote("git@github.com:user/repo.git"),
            Some(ssh(Some("git"), "github.com", None))
        );
        assert_eq!(
            ssh_remote("aur@aur.archlinux.org:paru"),
            Some(ssh(Some("aur"), "aur.archlinux.org", None))
        );
        assert_eq!(ssh_remote("host:repo.git"), Some(ssh(None, "host", None)));
        // The account ends at the first `@` before the first colon.
        assert_eq!(
            ssh_remote("a@b@host:repo.git"),
            Some(ssh(Some("a"), "b@host", None))
        );
        assert_eq!(
            ssh_remote("git@gitlab.com:group/sub/repo.git"),
            Some(ssh(Some("git"), "gitlab.com", None))
        );
    }

    /// What libgit2 does not reach over SSH, and what it reaches over SSH
    /// with no host to name, are told apart: the second is refused before
    /// connecting rather than connected to without a host-key check.
    #[test]
    fn transports_are_picked_as_libgit2_picks_them() {
        for url in [
            "https://github.com/user/repo.git",
            "HTTPS://github.com/user/repo.git",
            "http://user@host:8080/repo.git",
            "git://host/repo",
            "file:///srv/git/repo.git",
            "/srv/git/repo.git",
            "paru-git",
        ] {
            assert_eq!(transport(url), Transport::Other, "{url}");
        }
        for url in [
            // Any colon sends a URL over SSH: to libgit2 these are hosts
            // called `./a`, `/srv/a` and `dir/a`.
            "./a:b",
            "/srv/a:b",
            "dir/a:b",
            // A scheme it has no transport for.
            "nope://host/repo",
            "git+https://host/repo",
            // A host it would percent-decode.
            "ssh://ho%73t/repo",
            "ssh:///repo",
            "host:",
        ] {
            assert_eq!(transport(url), Transport::UnreadableSsh, "{url}");
        }
        assert!(matches!(
            transport("SSH://host/repo"),
            Transport::Ssh(SshRemote { host, .. }) if host == "host"
        ));
    }

    #[test]
    fn other_remotes_are_not_ssh_remotes() {
        for url in [
            "https://github.com/user/repo.git",
            "http://user@host:8080/repo.git",
            "git://host/repo",
            "file:///srv/git/repo.git",
            "/srv/git/repo.git",
            // A "host" with a slash in it is a path.
            "./a:b",
            "/srv/a:b",
            "dir/a:b",
            "paru-git",
            // Not a port: refused rather than guessed at.
            "ssh://host:ssh/repo",
            "ssh:///repo",
            "ssh://host:+22/repo",
            "@:repo",
            // libgit2 refuses this.
            "host:",
            // The bracketed scp-like form.
            "[git@host:2222]:repo",
            "git@[::1]:repo",
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
