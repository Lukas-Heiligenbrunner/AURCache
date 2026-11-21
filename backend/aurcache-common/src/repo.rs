//! How the pacman repository is addressed.
//!
//! The server knows how the repository is *published* — scheme, port, path,
//! whatever `AURCACHE_PUBLIC_URL` says, possibly through a reverse proxy. It
//! does not reliably know the *host* each client reaches it by: a worker in the
//! compose network, one on the LAN and a laptop running `pacman -Sy` all get
//! there by different names.
//!
//! So the split is always the same, and it is worth having in one place: take
//! everything but the host from the server, and the host from whoever is asking.

/// The default repository name. The server can override it with
/// `AURCACHE_REPO_NAME`; database filenames and pacman sections must agree.
pub const DEFAULT_REPO_NAME: &str = "repo";

/// A repository name must be both a filename component and a pacman section.
/// Keep configuration directives, paths and pacman's special section out.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty()
        || matches!(name, "options" | "local")
        || !name.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
    {
        return Err(
            "repository name must start with an ASCII letter, digit or underscore, contain only ASCII letters, digits, underscores, dots or hyphens, and must not be 'options' or 'local'",
        );
    }
    Ok(())
}

/// The `Server =` line for a repository published at `public_url`, as reached by
/// a client that gets to the instance at `host`. `listen_port` is the port the
/// repository listens on, which a `public_url` naming no port means.
///
/// `$arch` is appended for pacman to expand, so one line serves every
/// architecture the instance builds.
#[must_use]
pub fn server_url_for_host(public_url: &str, host: &str, listen_port: u16) -> String {
    let trimmed = public_url.trim_end_matches('/');
    let (scheme, rest) = trimmed
        .split_once("://")
        .map_or(("http", trimmed), |(scheme, rest)| (scheme, rest));
    let (authority, path) = rest
        .find('/')
        .map_or((rest, ""), |i| (&rest[..i], &rest[i..]));

    // A URL without an explicit port still needs one, or the result points at
    // the web UI instead of the repository. A bracketed `[v6]` authority has
    // colons of its own, so the port is read past the `]`, not after the
    // last `:` (which would read `:1]` out of `[::1]` as a port).
    let port = if let Some(inside) = authority.strip_prefix('[') {
        inside
            .split_once("]:")
            .map_or_else(|| format!(":{listen_port}"), |(_, p)| format!(":{p}"))
    } else {
        authority
            .rsplit_once(':')
            .map_or_else(|| format!(":{listen_port}"), |(_, p)| format!(":{p}"))
    };

    // Brackets for a bare IPv6 literal: without them the port merges into
    // the address and pacman reads garbage.
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };

    format!("{scheme}://{host}{port}{path}/$arch")
}

/// The host in `url`, without scheme, userinfo, port or path.
///
/// The one operation worth sharing: the server publishes one template for
/// every worker and the CLI derives sibling addresses, but the host is the
/// one part each of them reaches the instance by — so each pulls it back out
/// of the URL it already knows.
///
/// An IPv6 literal keeps its brackets: that is how it has to be written back
/// into a URL, and dropping them would produce an address that does not parse.
#[must_use]
pub fn host_from_url(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next()?;
    // Userinfo is delimited by the *last* `@`, since a password may contain one.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);

    let host = if authority.starts_with('[') {
        &authority[..=authority.find(']')?]
    } else {
        authority.split(':').next()?
    };

    (!host.is_empty()).then_some(host)
}

/// Where the API at `api_url` serves the repository's public key.
#[must_use]
pub fn key_url(api_url: &str) -> String {
    format!("{}/repo/key", api_url.trim_end_matches('/'))
}

/// The commands that make pacman trust a signed repository's key, as someone
/// would type them, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyTrustCommands {
    /// Add the key, fetched from where the server serves it, to pacman's
    /// keyring. `--add -` reads it from the pipe: `pacman-key --add` with no
    /// file at all refuses ("No targets specified").
    pub add: String,
    /// Locally sign it by the fingerprint the server reported, which is what
    /// makes pacman trust it. A key served that is not that one is added but
    /// never trusted.
    pub lsign: String,
}

impl KeyTrustCommands {
    /// For the key `key_url` serves, whose fingerprint is `fingerprint`.
    #[must_use]
    pub fn new(key_url: &str, fingerprint: &str) -> Self {
        Self {
            add: format!(
                "curl -fsSL {} | sudo pacman-key --add -",
                crate::shell::quote_argument(key_url)
            ),
            lsign: format!(
                "sudo pacman-key --lsign-key {}",
                crate::shell::quote_argument(fingerprint)
            ),
        }
    }

    /// The commands, in the order they are run.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        [self.add.as_str(), self.lsign.as_str()].into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyTrustCommands, host_from_url, key_url, server_url_for_host, validate_name};

    #[test]
    fn repository_names_are_safe_filenames_and_pacman_sections() {
        for name in ["repo", "my-packages", "custom_name.2", "_private"] {
            assert!(validate_name(name).is_ok(), "{name:?}");
        }
        for name in [
            "",
            "options",
            "local",
            ".hidden",
            "..",
            "../repo",
            "/repo",
            "a/b",
            "a\\b",
            "repo]",
            "a\nServer = bad",
            "a b",
            "a#b",
            "répo",
            "a\0b",
        ] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn the_key_is_fetched_and_signed_by_its_fingerprint() {
        let commands = KeyTrustCommands::new(
            &key_url("http://aurcache.example.com:8080/api/"),
            "0123456789ABCDEF0123456789ABCDEF01234567",
        );
        assert_eq!(
            commands.add,
            "curl -fsSL http://aurcache.example.com:8080/api/repo/key | sudo pacman-key --add -"
        );
        assert_eq!(
            commands.lsign,
            "sudo pacman-key --lsign-key 0123456789ABCDEF0123456789ABCDEF01234567"
        );
        assert_eq!(
            commands.iter().collect::<Vec<_>>(),
            [commands.add.as_str(), commands.lsign.as_str()]
        );
    }

    /// Only the host changes: the rest describes how the repository is
    /// published and stays the server's decision.
    #[test]
    fn replaces_only_the_host() {
        assert_eq!(
            server_url_for_host("http://aurcache.example.com:9000/repo", "HOST", 9081),
            "http://HOST:9000/repo/$arch"
        );
    }

    #[test]
    fn supplies_the_repository_port_when_the_url_omits_it() {
        assert_eq!(
            server_url_for_host("http://aurcache.example.com", "HOST", 9081),
            "http://HOST:9081/$arch"
        );
    }

    #[test]
    fn keeps_https_and_ignores_a_trailing_slash() {
        assert_eq!(
            server_url_for_host("https://aurcache.example.com:8081/", "HOST", 9081),
            "https://HOST:8081/$arch"
        );
    }

    /// A URL someone typed without a scheme still has to produce a usable line.
    #[test]
    fn a_missing_scheme_reads_as_http() {
        assert_eq!(
            server_url_for_host("aurcache.example.com:8081", "HOST", 9081),
            "http://HOST:8081/$arch"
        );
    }

    /// A port is supplied when the URL omits one, because `http://host/$arch`
    /// would otherwise reach the web UI rather than the repository.
    ///
    /// Inherited from the worker template this was extracted from, and wrong
    /// for one case: a reverse proxy publishing on 443 under a path gets a
    /// spurious repository port. Setting `AURCACHE_PUBLIC_URL` with an explicit port
    /// (`https://host:443/arch`) is the workaround. Changing it would change
    /// what every worker is told, so it is deliberately not changed here.
    #[test]
    fn a_path_is_preserved_and_a_missing_port_is_supplied() {
        assert_eq!(
            server_url_for_host("https://aurcache.example.com/arch", "HOST", 9081),
            "https://HOST:9081/arch/$arch"
        );
    }

    /// An IPv6 host needs brackets, or the port merges into the address and
    /// pacman reads garbage.
    #[test]
    fn an_ipv6_host_is_bracketed() {
        assert_eq!(
            server_url_for_host("http://aurcache.example.com:9000/repo", "::1", 9081),
            "http://[::1]:9000/repo/$arch"
        );
    }

    /// ...and an IPv6 public URL keeps its own port rather than reading
    /// `:1]` out of `[::1]` as one.
    #[test]
    fn an_ipv6_public_url_keeps_its_port() {
        assert_eq!(
            server_url_for_host("http://[fd00::1]:9000/repo", "worker", 9081),
            "http://worker:9000/repo/$arch"
        );
        assert_eq!(
            server_url_for_host("http://[fd00::1]/repo", "worker", 9081),
            "http://worker:9081/repo/$arch"
        );
    }

    #[test]
    fn a_host_is_read_out_of_the_usual_shapes() {
        assert_eq!(
            host_from_url("http://localhost:8080/api"),
            Some("localhost")
        );
        assert_eq!(
            host_from_url("https://aurcache.example.com"),
            Some("aurcache.example.com")
        );
        assert_eq!(
            host_from_url("http://192.168.1.10:8080/"),
            Some("192.168.1.10")
        );
        assert_eq!(host_from_url("https://10.0.0.5:8083/api"), Some("10.0.0.5"));
        // No scheme at all: the whole thing is the authority.
        assert_eq!(host_from_url("localhost:8080/api"), Some("localhost"));
        assert_eq!(host_from_url("aurcache:8083"), Some("aurcache"));
    }

    /// The brackets are part of the address once it goes back into a URL.
    #[test]
    fn an_ipv6_literal_keeps_its_brackets() {
        assert_eq!(host_from_url("http://[::1]:8080/api"), Some("[::1]"));
        assert_eq!(
            host_from_url("http://[2001:db8::1]/api"),
            Some("[2001:db8::1]")
        );
        assert_eq!(host_from_url("https://[fd00::1]:8083"), Some("[fd00::1]"));
    }

    /// An unclosed bracket is no host at all, not a prefix of one: splitting
    /// `"[::1"` on `:` would otherwise hand back `"["`.
    #[test]
    fn an_unclosed_ipv6_literal_has_no_host() {
        assert_eq!(host_from_url("http://[::1:8080/api"), None);
    }

    /// A password may itself contain `@`, so the split has to be from the right.
    #[test]
    fn userinfo_is_stripped_from_the_last_separator() {
        assert_eq!(
            host_from_url("http://user:p@ss@host:8080/api"),
            Some("host")
        );
    }

    #[test]
    fn a_url_without_a_host_has_none() {
        assert_eq!(host_from_url(""), None);
        assert_eq!(host_from_url("http://"), None);
        assert_eq!(host_from_url("http:///api"), None);
    }
}
