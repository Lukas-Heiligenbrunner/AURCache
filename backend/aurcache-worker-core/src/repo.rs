//! Rendering the `[repo]` section a build uses to resolve AURCache packages.
//!
//! The server sends a template at registration with the host left as
//! [`REPO_HOST_PLACEHOLDER`], because the host is the one part it cannot know:
//! workers reach the same server by different addresses — a compose service
//! name, a LAN address, `localhost` for a worker embedded beside it — and a
//! single baked-in value is wrong for all but one of them. Everything else
//! about the section stays the server's choice.
//!
//! The worker already knows the right answer: it is the host it dialled to
//! reach the server in the first place.

use aurcache_common::repo::host_from_url;
use aurcache_common::worker::REPO_HOST_PLACEHOLDER;

/// Render the server's template for this worker.
///
/// An empty result means no `[repo]` section: either the server publishes no
/// repository, or no host could be determined. Emitting a section containing an
/// unresolved placeholder would be worse — pacman would fail on a URL nobody
/// wrote, instead of simply not knowing the repository.
///
/// `override_host` corresponds to `AURCACHE_REPO_HOST`, for deployments where
/// the address a worker uses for the protocol is not the address it should use
/// for the repository.
///
/// `override_url` corresponds to `AURCACHE_REPO_URL` and replaces the base URL
/// outright — scheme, host, port and path. The host-only override assumes every
/// worker reaches the repository on the same scheme, port and path as every
/// other, which is the server's single `AURCACHE_PUBLIC_URL`; a worker behind a
/// reverse proxy (`https://aur.example.com/repo`) beside one on the compose
/// network (`http://aurcache:8081`) breaks that assumption. It wins over
/// `override_host`, being the same knob with a longer reach.
#[must_use]
pub fn render(
    template: &str,
    server_url: &str,
    override_host: Option<&str>,
    override_url: Option<&str>,
) -> String {
    if template.trim().is_empty() {
        return String::new();
    }
    if let Some(base) = override_url.map(str::trim).filter(|u| !u.is_empty()) {
        return with_base_url(template, base);
    }
    let host = match override_host.map(str::trim).filter(|h| !h.is_empty()) {
        Some(host) => host.to_string(),
        None => match host_from_url(server_url) {
            Some(host) => host.to_owned(),
            None => return String::new(),
        },
    };
    template.replace(REPO_HOST_PLACEHOLDER, &host)
}

/// Point the template's `Server =` line at `base`, keeping the rest of it.
///
/// The rest is the server's to decide — `SigLevel`, and whatever else it may
/// put there — so only the one line the override is about is rewritten.
///
/// `/$arch` is appended because that is what the server does when it builds the
/// line from `AURCACHE_PUBLIC_URL`, and someone setting a base URL is answering
/// the same question. A `$arch` already present is left alone, so spelling it
/// out is not punished.
fn with_base_url(template: &str, base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    let server = if trimmed.contains("$arch") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/$arch")
    };
    let mut out = String::new();
    for line in template.lines() {
        if line.trim_start().starts_with("Server =") {
            out.push_str("Server = ");
            out.push_str(&server);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// URL of the repository database for one arch, given a rendered `[repo]`
/// section.
///
/// pacman fetches `<Server>/<section-name>.db`, so the DB filename comes from
/// the section header (`[repo]` behaves as `repo.db`) rather than being
/// hardcoded — a section renamed elsewhere keeps working. `$arch` is
/// substituted wherever it appears before appending `<name>.db`.
///
/// Parsed as a proper URL rather than patched by string: only an `http(s)`
/// URL with a host is accepted, and a query string or fragment is rejected
/// because the `<name>.db` suffix would have to be guessed past it. A `None`
/// result means "no reconciliation", never a URL nobody wrote.
#[must_use]
pub fn repo_db_url(section: &str, arch: &str) -> Option<String> {
    // The name and the server line belong together: the DB filename comes
    // from the header of the section *containing* the `Server =` line, not
    // from the first header anywhere in the file. Pairing them independently
    // misresolves the moment another section (e.g. `[options]`) precedes it.
    let mut name: Option<&str> = None;
    let mut server: Option<&str> = None;
    for line in section.lines() {
        let trimmed = line.trim();
        if let Some(header) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.split(']').next())
            .filter(|n| !n.is_empty())
        {
            name = Some(header);
        } else if let Some(s) = trimmed
            .strip_prefix("Server =")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            // Stop here: a header further down belongs to another section.
            server = Some(s);
            break;
        }
    }
    let (name, server) = (name?, server?);
    let mut url = url::Url::parse(server).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return None;
    }
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let base = url.path().replace("$arch", arch);
    let base = base.trim_end_matches('/');
    url.set_path(&format!("{base}/{name}.db"));
    Some(url.to_string())
}

/// Append a rendered `[repo]` section to a job's `pacman.conf`.
///
/// An empty section is appended as nothing.
#[must_use]
pub fn append_to_pacman_conf(pacman_conf: &str, section: &str) -> String {
    if section.trim().is_empty() {
        return pacman_conf.to_string();
    }
    let mut out = pacman_conf.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(section);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host-only override cannot express a worker that reaches the
    /// repository on a different port or scheme from every other worker --
    /// which is what a reverse proxy in front of one of them means.
    #[test]
    fn a_base_url_override_replaces_more_than_the_host() {
        let template = "[repo]\nSigLevel = Never\nServer = http://%AURCACHE_HOST%:8081/$arch\n";

        let proxied = render(
            template,
            "https://aurcache:8083",
            None,
            Some("https://aur.example.com/repo"),
        );

        assert!(proxied.contains("Server = https://aur.example.com/repo/$arch"));
        // Everything the server decided that is not the URL survives.
        assert!(proxied.starts_with("[repo]\nSigLevel = Never\n"));
        assert!(!proxied.contains(REPO_HOST_PLACEHOLDER));
        assert!(!proxied.contains(":8081"));
    }

    /// The published port and the in-network port differ whenever compose maps
    /// one to the other. This is the case that sent a worker to a port nothing
    /// was listening on.
    #[test]
    fn a_base_url_override_can_change_only_the_port() {
        let template = "[repo]\nSigLevel = Never\nServer = http://%AURCACHE_HOST%:8091/$arch\n";

        let rendered = render(
            template,
            "https://aurcache:8083",
            None,
            Some("http://aurcache:8081"),
        );

        assert!(rendered.contains("Server = http://aurcache:8081/$arch"));
    }

    /// It is the same knob as the host override with a longer reach, so it
    /// wins rather than combining with it.
    #[test]
    fn the_base_url_override_beats_the_host_override() {
        let template = "Server = http://%AURCACHE_HOST%:8081/$arch\n";

        let rendered = render(
            template,
            "https://aurcache:8083",
            Some("repo.internal"),
            Some("http://elsewhere:9000"),
        );

        assert!(rendered.contains("Server = http://elsewhere:9000/$arch"));
        assert!(!rendered.contains("repo.internal"));
    }

    /// Spelling out `$arch` is what someone copying the existing line would
    /// do, and appending a second one would produce a URL that 404s.
    #[test]
    fn an_arch_already_in_the_override_is_left_alone() {
        let template = "Server = http://%AURCACHE_HOST%:8081/$arch\n";

        let rendered = render(
            template,
            "https://aurcache:8083",
            None,
            Some("http://h/r/$arch"),
        );

        assert_eq!(rendered, "Server = http://h/r/$arch\n");
    }

    /// A blank value is an unset one, matching every other override here.
    #[test]
    fn a_blank_base_url_override_is_ignored() {
        let template = "Server = http://%AURCACHE_HOST%:8081/$arch\n";

        let rendered = render(template, "https://aurcache:8083", None, Some("   "));

        assert!(rendered.contains("Server = http://aurcache:8081/$arch"));
    }

    /// Each worker must get the host *it* uses, which is the entire point of
    /// templating rather than baking the URL in server-side.
    #[test]
    fn renders_a_different_host_per_worker() {
        let template = "[repo]\nSigLevel = Never\nServer = http://%AURCACHE_HOST%:8081/$arch\n";
        let compose = render(template, "https://aurcache:8083", None, None);
        let lan = render(template, "https://aur.example.com:8083", None, None);
        let embedded = render(template, "https://localhost:8083", None, None);

        assert!(compose.contains("Server = http://aurcache:8081/$arch"));
        assert!(lan.contains("Server = http://aur.example.com:8081/$arch"));
        assert!(embedded.contains("Server = http://localhost:8081/$arch"));
        // The port and section name stay exactly as the server chose them.
        for rendered in [&compose, &lan, &embedded] {
            assert!(rendered.starts_with("[repo]\nSigLevel = Never\n"));
            assert!(!rendered.contains(REPO_HOST_PLACEHOLDER));
        }
    }

    #[test]
    fn an_explicit_override_wins() {
        let template = "Server = http://%AURCACHE_HOST%:8081/$arch\n";
        let rendered = render(
            template,
            "https://aurcache:8083",
            Some("repo.internal"),
            None,
        );
        assert!(rendered.contains("http://repo.internal:8081"));
        // Blank is treated as unset rather than as an empty host.
        let blank = render(template, "https://aurcache:8083", Some("  "), None);
        assert!(blank.contains("http://aurcache:8081"));
    }

    /// Producing a section with an unresolved placeholder would hand pacman a
    /// URL it cannot use; producing none lets the build fail with a clear
    /// "unknown repo" instead.
    #[test]
    fn no_host_yields_no_section() {
        assert!(render("Server = %AURCACHE_HOST%", "://", None, None).is_empty());
    }

    /// A server that publishes no repository sends an empty template, and that
    /// must not become a section containing nothing useful.
    #[test]
    fn an_empty_template_renders_to_nothing() {
        assert!(render("", "https://aurcache:8083", None, None).is_empty());
    }

    #[test]
    fn appending_leaves_the_config_alone_when_there_is_no_section() {
        assert_eq!(append_to_pacman_conf("[options]\n", ""), "[options]\n");
        let with = append_to_pacman_conf("[options]", "[repo]\n");
        assert!(with.starts_with("[options]\n"));
        assert!(with.ends_with("[repo]\n"));
    }

    /// The DB URL is the `Server` line with `$arch` substituted and the
    /// section-derived `<name>.db` appended.
    #[test]
    fn db_url_substitutes_arch_and_appends_the_section_db() {
        let section =
            "[repo]\nSigLevel = Never\nServer = https://aur.example.com:9000/repo/$arch\n";
        assert_eq!(
            repo_db_url(section, "x86_64").as_deref(),
            Some("https://aur.example.com:9000/repo/x86_64/repo.db")
        );
    }

    /// A renamed section names its own DB, since pacman derives the DB
    /// filename from the section this way.
    #[test]
    fn db_url_uses_the_section_name_not_a_hardcoded_repo() {
        let section = "[myrepo]\nServer = http://host:8081/$arch\n";
        assert_eq!(
            repo_db_url(section, "aarch64").as_deref(),
            Some("http://host:8081/aarch64/myrepo.db")
        );
    }

    /// The name comes from the section holding the `Server =` line, not the
    /// first header in the file: a real `pacman.conf` has `[options]` (and
    /// friends) above the appended `[repo]` section, which used to resolve
    /// to `options.db` and skip every reconcile with a 404.
    #[test]
    fn db_url_pairs_the_server_line_with_its_own_section() {
        let conf = "[options]\nHoldPkg = pacman\n[repo]\nServer = http://host:8081/$arch\n";
        assert_eq!(
            repo_db_url(conf, "x86_64").as_deref(),
            Some("http://host:8081/x86_64/repo.db")
        );
        // Nor from a header that follows it.
        let conf = "[repo]\nServer = http://host:8081/$arch\n[extra]\nInclude = x\n";
        assert_eq!(
            repo_db_url(conf, "x86_64").as_deref(),
            Some("http://host:8081/x86_64/repo.db")
        );
    }

    /// pacman does not care which of `/$arch` and `/$arch/` a template ends
    /// in; the DB URL must not gain a double slash either way.
    #[test]
    fn db_url_without_double_slashes() {
        let trailing = "[repo]\nServer = http://host:8081/$arch/\n";
        assert_eq!(
            repo_db_url(trailing, "x86_64").as_deref(),
            Some("http://host:8081/x86_64/repo.db")
        );
    }

    /// A `Server` line that spells the arch out already (someone's copy-paste)
    /// is left alone rather than gaining a second one.
    #[test]
    fn db_url_leaves_a_literal_arch_alone() {
        let section = "[repo]\nServer = http://h/r/i486\n";
        assert_eq!(
            repo_db_url(section, "i486").as_deref(),
            Some("http://h/r/i486/repo.db")
        );
    }

    /// A query string or fragment would make the `<name>.db` suffix ambiguous;
    /// refusing beats fetching a URL nobody wrote.
    #[test]
    fn db_url_rejects_query_and_fragment() {
        let query = "[repo]\nServer = http://h/$arch?token=1\n";
        assert_eq!(repo_db_url(query, "x86_64"), None);
        let fragment = "[repo]\nServer = http://h/$arch#frag\n";
        assert_eq!(repo_db_url(fragment, "x86_64"), None);
    }

    /// A non-http server or a bare-hostless string is not something we can
    /// fetch a DB from.
    #[test]
    fn db_url_rejects_non_http_and_hostless() {
        assert_eq!(
            repo_db_url("[repo]\nServer = ftp://h/$arch\n", "x86_64"),
            None
        );
        assert_eq!(repo_db_url("[repo]\nServer = $arch\n", "x86_64"), None);
        assert_eq!(repo_db_url("[repo]\nServer =\n", "x86_64"), None);
    }

    /// A section that does not name itself or its server yields nothing; a
    /// blank section is an unset one.
    #[test]
    fn db_url_requires_a_named_section_and_server() {
        assert_eq!(repo_db_url("[options]\n", "x86_64"), None);
        assert_eq!(repo_db_url("", "x86_64"), None);
    }
}
