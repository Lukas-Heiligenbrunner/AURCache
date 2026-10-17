//! `aurcli repo config` — the `pacman.conf` stanza for this instance.
//!
//! The last step of setting AURCache up is consuming it, and the documentation
//! can only print a stanza with `<server_ip>` left as an exercise. The CLI
//! already knows the address, because it is talking to the thing: the repo is
//! the same host on the mirror port. So this command prints the finished block
//! with nothing left to substitute.

use crate::url::{is_loopback, scheme_from_url};
use anyhow::{Context, Result, anyhow, bail};
use aurcache_common::ports::AURCACHE_MIRROR_PORT;
use aurcache_common::repo::host_from_url;
use serde::Serialize;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// What the server names the repository, and therefore what pacman must call
/// it: the database files are `repo.db` / `repo.files`, so the section header
/// is not a free choice.
pub use aurcache_common::repo::REPO_NAME as DEFAULT_REPO_NAME;

/// Matches the documented stanza. AURCache does not sign its packages, so a
/// stricter level would refuse everything it serves.
pub const DEFAULT_SIGLEVEL: &str = "Optional TrustAll";

#[derive(Debug, Clone, Serialize)]
pub struct RepoConfig {
    pub name: String,
    pub server: String,
    pub siglevel: String,
    /// The three lines as they go into `pacman.conf`, so a JSON consumer does
    /// not have to reassemble them and get the order or spacing wrong.
    pub block: String,
}

/// The `Server =` value for the repository published by the instance at
/// `api_url`.
///
/// `$arch` is pacman's own variable and is left for pacman to expand — one
/// stanza serves every architecture the instance builds for.
pub fn repo_server_url(api_url: &str, port: u16) -> Result<String> {
    let host = host_from_url(api_url)
        .ok_or_else(|| anyhow!("could not read a host out of the API URL `{api_url}`"))?;
    Ok(format!(
        "{}://{host}:{port}/$arch",
        scheme_from_url(api_url)
    ))
}

/// The `Server =` value for an instance that reported publishing at
/// `public_url`, as reached from here at `api_url`.
///
/// Scheme, port and path always come from the server: it is the only party that
/// knows whether the repository sits on a non-default port, under a path, or
/// behind a reverse proxy. The host is decided here, because that is the part
/// the server cannot know — it sees only how it was reached, not how the
/// machine editing `pacman.conf` gets there.
pub fn server_url_from_public(public_url: &str, api_url: &str) -> Result<String> {
    let host = match host_from_url(public_url) {
        // A published host naming a real address is a deliberate statement of
        // how clients should reach the repository, and outranks whatever
        // address we happen to be talking to.
        Some(published) if !is_loopback(published) => published.to_string(),
        // `localhost` is the unconfigured default and is only ever right on the
        // server itself. Substitute the address that demonstrably works here.
        _ => host_from_url(api_url)
            .ok_or_else(|| anyhow!("could not read a host out of the API URL `{api_url}`"))?
            .to_string(),
    };
    Ok(aurcache_common::repo::server_url_for_host(
        public_url, &host,
    ))
}

/// Build the stanza for the instance at `api_url`.
///
/// `published` is what the server said about itself, when it could be asked.
/// An explicit `port` outranks it: someone passing `--port` is correcting us,
/// not asking to be corrected.
pub fn repo_config(
    api_url: &str,
    port: Option<u16>,
    name: Option<String>,
    siglevel: Option<String>,
    published: Option<&str>,
) -> Result<RepoConfig> {
    let server = match (port, published) {
        (None, Some(public_url)) => server_url_from_public(public_url, api_url)?,
        (port, _) => repo_server_url(api_url, port.unwrap_or(AURCACHE_MIRROR_PORT))?,
    };
    let name = name.unwrap_or_else(|| DEFAULT_REPO_NAME.to_string());
    let siglevel = siglevel.unwrap_or_else(|| DEFAULT_SIGLEVEL.to_string());
    let block = format!("[{name}]\nSigLevel = {siglevel}\nServer = {server}\n");
    Ok(RepoConfig {
        name,
        server,
        siglevel,
        block,
    })
}

/// The system pacman configuration, which is where a stanza has to end up.
pub const PACMAN_CONF: &str = "/etc/pacman.conf";

/// Whether `conf` already declares a `[name]` repository.
///
/// Appending a second copy of a section pacman already has is not a harmless
/// no-op — it warns, and the duplicate outlives whatever made it — so this is
/// what makes `--install` safe to run twice.
#[must_use]
pub fn has_section(conf: &str, name: &str) -> bool {
    let header = format!("[{name}]");
    conf.lines().any(|line| {
        let line = line.trim();
        // A commented-out section is not a section: someone who disabled it
        // gets it back, rather than a confusing "already configured".
        !line.starts_with('#') && line == header
    })
}

/// `conf` with `block` appended, separated by exactly one blank line.
///
/// pacman does not care about the blank line; a human reading the file after us
/// does, and a file that does not end in a newline would otherwise splice our
/// header onto their last line.
#[must_use]
pub fn append_stanza(conf: &str, block: &str) -> String {
    let mut out = conf.to_string();
    if !out.is_empty() {
        while out.ends_with('\n') {
            out.pop();
        }
        out.push_str("\n\n");
    }
    out.push_str(block);
    out
}

/// Write the stanza into `path`, elevating only if we have to.
///
/// Tries the unprivileged write first: running as root, or against a file the
/// user owns, then never prompts for a password. Only a `PermissionDenied`
/// falls back to `sudo`, which prompts on the terminal — a process cannot raise
/// its own privileges, but it can hand the work to one that has them.
pub fn install_stanza(path: &Path, config: &RepoConfig) -> Result<Installed> {
    let existing = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            bail!("{} does not exist; is this an Arch system?", path.display())
        }
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("failed to read {}", path.display()));
        }
    };

    if has_section(&existing, &config.name) {
        return Ok(Installed {
            path: path.display().to_string(),
            changed: false,
            elevated: false,
        });
    }

    let updated = append_stanza(&existing, &config.block);
    match fs::write(path, &updated) {
        Ok(()) => Ok(Installed {
            path: path.display().to_string(),
            changed: true,
            elevated: false,
        }),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            append_via_sudo(path, &config.block)?;
            Ok(Installed {
                path: path.display().to_string(),
                changed: true,
                elevated: true,
            })
        }
        Err(e) => Err(anyhow::Error::new(e))
            .with_context(|| format!("failed to write {}", path.display())),
    }
}

/// Append through `sudo tee`, which prompts on the terminal.
///
/// `tee -a` rather than rewriting the whole file: appending touches only what we
/// add, so a failure part-way cannot lose a configuration we did not write.
fn append_via_sudo(path: &Path, block: &str) -> Result<()> {
    let mut child = Command::new("sudo")
        .arg("tee")
        .arg("-a")
        .arg(path)
        .stdin(Stdio::piped())
        // tee echoes what it writes; the caller already showed the stanza.
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                anyhow::anyhow!(
                    "cannot write {} and sudo is not installed; append the stanza yourself",
                    path.display()
                )
            } else {
                anyhow::Error::new(e).context("failed to run sudo")
            }
        })?;

    child
        .stdin
        .take()
        .context("sudo stdin was not available")?
        .write_all(format!("\n{block}").as_bytes())
        .context("failed to send the stanza to sudo")?;

    let status = child.wait().context("waiting for sudo")?;
    if !status.success() {
        bail!("sudo did not write {}", path.display());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Installed {
    pub path: String,
    /// False when the section was already present, which is the common case on
    /// a second run.
    pub changed: bool,
    /// Whether it needed `sudo`.
    pub elevated: bool,
}

/// Print the stanza on stdout and the what-to-do-with-it on stderr.
///
/// Split that way on purpose: `aurcli repo config >> pacman.conf` has to
/// append three usable lines and nothing else, so the instructions cannot share
/// the stream with them.
pub fn print_repo_config(config: &RepoConfig) {
    print!("{}", config.block);
    eprintln!();
    eprintln!("Add it to /etc/pacman.conf (asks for sudo only if it has to):");
    eprintln!("  aurcli repo config --install");
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_REPO_NAME, append_stanza, has_section, install_stanza, repo_config, repo_server_url,
    };

    #[test]
    fn the_repo_is_the_api_host_on_the_mirror_port() {
        let server = repo_server_url("http://192.168.1.10:8080/api", 8081).unwrap();
        assert_eq!(server, "http://192.168.1.10:8081/$arch");
    }

    /// A TLS-terminated API means a TLS-terminated repo on the same host; the
    /// alternative is emitting a URL that a reverse-proxied deployment cannot
    /// reach.
    #[test]
    fn the_scheme_follows_the_api_url() {
        let server = repo_server_url("https://aurcache.example.com/api", 8081).unwrap();
        assert_eq!(server, "https://aurcache.example.com:8081/$arch");
    }

    #[test]
    fn an_ipv6_host_stays_bracketed() {
        let server = repo_server_url("http://[::1]:8080/api", 8081).unwrap();
        assert_eq!(server, "http://[::1]:8081/$arch");
    }

    /// `$arch` belongs to pacman: one stanza has to serve every architecture
    /// the instance builds, so the CLI must not expand it.
    #[test]
    fn the_block_is_three_lines_and_keeps_the_arch_variable() {
        let config = repo_config("http://localhost:8080/api", None, None, None, None).unwrap();
        let lines: Vec<&str> = config.block.lines().collect();
        assert_eq!(lines.len(), 3, "{:?}", config.block);
        assert_eq!(lines[0], format!("[{DEFAULT_REPO_NAME}]"));
        assert!(lines[1].starts_with("SigLevel = "), "{:?}", lines[1]);
        assert!(lines[2].ends_with("/$arch"), "{:?}", lines[2]);
        assert!(config.block.ends_with('\n'), "must append cleanly");
    }

    #[test]
    fn the_name_port_and_siglevel_can_be_overridden() {
        let config = repo_config(
            "http://localhost:8080/api",
            Some(9000),
            Some("mine".to_string()),
            Some("Never".to_string()),
            None,
        )
        .unwrap();
        assert!(config.block.contains("[mine]"), "{:?}", config.block);
        assert!(
            config.block.contains("SigLevel = Never"),
            "{:?}",
            config.block
        );
        assert!(config.server.contains(":9000/"), "{:?}", config.server);
    }

    #[test]
    fn a_url_without_a_host_is_an_error() {
        assert!(repo_server_url("http://", 8081).is_err());
    }

    /// What makes `--install` safe to run twice.
    #[test]
    fn an_existing_section_is_detected() {
        let conf = "[options]\nHoldPkg = pacman\n\n[repo]\nServer = http://x/$arch\n";
        assert!(has_section(conf, "repo"));
        assert!(!has_section(conf, "other"));
    }

    /// A section someone commented out is a section they turned off, and they
    /// should get it back rather than "already configured".
    #[test]
    fn a_commented_out_section_does_not_count() {
        assert!(!has_section("#[repo]\n", "repo"));
        assert!(!has_section("# [repo]\n", "repo"));
    }

    /// `[repo-testing]` is a different repository and must not be mistaken for
    /// `[repo]` by a prefix match.
    #[test]
    fn a_longer_name_is_not_the_same_section() {
        assert!(!has_section("[repo-testing]\n", "repo"));
    }

    #[test]
    fn a_section_is_found_despite_indentation() {
        assert!(has_section("  [repo]  \n", "repo"));
    }

    /// A file that does not end in a newline would otherwise get our header
    /// spliced onto its last line.
    #[test]
    fn appending_always_separates_with_one_blank_line() {
        let appended = append_stanza("[options]\nHoldPkg = pacman", "[repo]\n");
        assert_eq!(appended, "[options]\nHoldPkg = pacman\n\n[repo]\n");
    }

    #[test]
    fn trailing_newlines_are_collapsed_not_multiplied() {
        let appended = append_stanza("[options]\n\n\n", "[repo]\n");
        assert_eq!(appended, "[options]\n\n[repo]\n");
    }

    #[test]
    fn appending_to_an_empty_file_adds_no_leading_blank_line() {
        assert_eq!(append_stanza("", "[repo]\n"), "[repo]\n");
    }

    /// The unprivileged path: a writable file must never invoke sudo.
    #[test]
    fn a_writable_file_is_installed_without_elevation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacman.conf");
        std::fs::write(&path, "[options]\nHoldPkg = pacman\n").unwrap();

        let config = repo_config("http://localhost:8080/api", None, None, None, None).unwrap();
        let result = install_stanza(&path, &config).unwrap();

        assert!(result.changed);
        assert!(!result.elevated);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("[repo]"), "{written}");
        assert!(
            written.contains("Server = http://localhost:8081/$arch"),
            "{written}"
        );
    }

    /// Running it twice must not leave two `[repo]` sections behind.
    #[test]
    fn installing_twice_changes_nothing_the_second_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacman.conf");
        std::fs::write(&path, "[options]\n").unwrap();

        let config = repo_config("http://localhost:8080/api", None, None, None, None).unwrap();
        assert!(install_stanza(&path, &config).unwrap().changed);
        let once = std::fs::read_to_string(&path).unwrap();

        assert!(!install_stanza(&path, &config).unwrap().changed);
        assert_eq!(once, std::fs::read_to_string(&path).unwrap());
        assert_eq!(once.matches("[repo]").count(), 1, "{once}");
    }

    /// The bug this exists to fix: a deployment publishing on a non-default
    /// port was previously given 8081 regardless.
    #[test]
    fn the_servers_port_and_path_are_taken_over_the_default() {
        let config = repo_config(
            "http://192.168.1.5:8080/api",
            None,
            None,
            None,
            Some("http://localhost:9000/arch"),
        )
        .unwrap();
        assert_eq!(config.server, "http://192.168.1.5:9000/arch/$arch");
    }

    /// A published host that names a real address was configured deliberately —
    /// a reverse proxy, say — and the client should use it rather than the
    /// address we happen to be talking to.
    #[test]
    fn a_real_published_host_outranks_the_api_host() {
        let config = repo_config(
            "http://192.168.1.5:8080/api",
            None,
            None,
            None,
            Some("https://aurcache.example.com/repo"),
        )
        .unwrap();
        assert_eq!(
            config.server,
            "https://aurcache.example.com:8081/repo/$arch"
        );
    }

    /// `localhost` is the unconfigured default: right only on the server
    /// itself, so the address that works from here replaces it.
    #[test]
    fn a_localhost_publication_is_replaced_with_a_usable_host() {
        let config = repo_config(
            "http://192.168.1.5:8080/api",
            None,
            None,
            None,
            Some("http://localhost:8081"),
        )
        .unwrap();
        assert_eq!(config.server, "http://192.168.1.5:8081/$arch");
    }

    /// Someone passing `--port` is correcting us, not asking to be corrected.
    #[test]
    fn an_explicit_port_outranks_the_servers_answer() {
        let config = repo_config(
            "http://192.168.1.5:8080/api",
            Some(9999),
            None,
            None,
            Some("http://localhost:9000"),
        )
        .unwrap();
        assert!(config.server.contains(":9999/"), "{}", config.server);
    }

    #[test]
    fn a_missing_pacman_conf_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo_config("http://localhost:8080/api", None, None, None, None).unwrap();
        let err = install_stanza(&dir.path().join("nope.conf"), &config).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }
}
