//! `aurcli repo config` — the `pacman.conf` stanza for this instance.
//!
//! The last step of setting AURCache up is consuming it, and the documentation
//! can only print a stanza with `<server_ip>` left as an exercise. The CLI
//! already knows the address, because it is talking to the thing: the repo is
//! the same host on the repository port. So this command prints the finished
//! block with nothing left to substitute.

use crate::url::{is_loopback, scheme_from_url};
use anyhow::{Context, Result, anyhow, bail};
use aurcache_common::api::repo::{RepoInfo, SigningInfo};
use aurcache_common::ports::AURCACHE_REPO_PORT;
use aurcache_common::repo::KeyTrustCommands;
use aurcache_common::repo::host_from_url;
use serde::Serialize;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// The name assumed offline or when a server predates configurable names.
pub use aurcache_common::repo::DEFAULT_REPO_NAME;

/// Matches the documented stanza, for an instance that does not sign -- or
/// is still signing what it published before -- so that a stricter level
/// would refuse what it serves unsigned.
pub const DEFAULT_SIGLEVEL: &str = "Optional TrustAll";

/// For an instance that signs every package and both databases.
pub const SIGNED_SIGLEVEL: &str = "Required";

#[derive(Debug, Clone, Serialize)]
pub struct RepoConfig {
    pub name: String,
    pub server: String,
    pub siglevel: String,
    /// The three lines as they go into `pacman.conf`, so a JSON consumer does
    /// not have to reassemble them and get the order or spacing wrong.
    pub block: String,
    /// The key the repository is signed with, when the server said it signs.
    /// pacman checks any signature it finds, even where they are optional, so
    /// it has to trust this key before it syncs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<RepoKey>,
}

/// The key a signed repository is signed with, and where to get it.
#[derive(Debug, Clone, Serialize)]
pub struct RepoKey {
    /// As `pacman-key --lsign-key` takes it.
    pub fingerprint: String,
    /// Where the armored public key is served, without authentication.
    pub url: String,
    /// Whether the instance is still signing what it published before it
    /// signed. Until it is done, the stanza keeps signatures optional.
    pub backfilling: bool,
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

/// The `Server =` value for an instance that reported how it publishes its
/// repository, as reached from here at `api_url`.
///
/// Scheme, port and path always come from the server: it is the only party that
/// knows whether the repository sits on a non-default port, under a path, or
/// behind a reverse proxy. The host is decided here, because that is the part
/// the server cannot know — it sees only how it was reached, not how the
/// machine editing `pacman.conf` gets there.
pub fn server_url_from_public(published: &RepoInfo, api_url: &str) -> Result<String> {
    let public_url = published.public_url.as_str();
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
    // A server too old to report its port listens on the default one.
    let listen_port = published.listen_port.unwrap_or(AURCACHE_REPO_PORT);
    Ok(aurcache_common::repo::server_url_for_host(
        public_url,
        &host,
        listen_port,
    ))
}

/// Build the stanza for the instance at `api_url`.
///
/// `published` is what the server said about itself, when it could be asked.
/// An explicit `port` outranks the address it gives: someone passing `--port`
/// is correcting us, not asking to be corrected. With neither, the repository
/// is assumed to be on the default port, which is only a guess: the server may
/// listen on another (`AURCACHE_REPO_PORT`). `signing` -- what the server
/// says about its key, which it says without a token too -- decides the
/// default `SigLevel`, which `siglevel` outranks in turn.
pub fn repo_config(
    api_url: &str,
    port: Option<u16>,
    name: Option<String>,
    siglevel: Option<String>,
    published: Option<&RepoInfo>,
    signing: Option<&SigningInfo>,
) -> Result<RepoConfig> {
    let server = match (port, published) {
        (None, Some(published)) => server_url_from_public(published, api_url)?,
        (port, _) => repo_server_url(api_url, port.unwrap_or(AURCACHE_REPO_PORT))?,
    };
    let key = signing.map(|signing| RepoKey {
        fingerprint: signing.fingerprint.clone(),
        url: aurcache_common::repo::key_url(api_url),
        backfilling: signing.backfilling,
    });
    let name = name.unwrap_or_else(|| {
        published
            .map_or(DEFAULT_REPO_NAME, |info| info.name.as_str())
            .to_string()
    });
    aurcache_common::repo::validate_name(&name).map_err(anyhow::Error::msg)?;
    let siglevel = siglevel.unwrap_or_else(|| {
        match &key {
            Some(key) if !key.backfilling => SIGNED_SIGLEVEL,
            _ => DEFAULT_SIGLEVEL,
        }
        .to_string()
    });
    let block = format!("[{name}]\nSigLevel = {siglevel}\nServer = {server}\n");
    Ok(RepoConfig {
        name,
        server,
        siglevel,
        block,
        key,
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
    // Matched whole, so a commented-out section is not a section: someone who
    // disabled it gets it back, rather than a confusing "already configured".
    conf.lines().any(|line| line.trim() == header)
}

/// The `SigLevel` the `[name]` section of `conf` sets, if it is there and
/// sets one.
#[must_use]
pub fn section_siglevel(conf: &str, name: &str) -> Option<String> {
    let header = format!("[{name}]");
    let mut lines = conf.lines().map(str::trim);
    lines.find(|line| *line == header)?;
    lines
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "SigLevel").then(|| value.trim().to_string())
        })
}

/// What a client set up for a signed repository should still change in
/// its `SigLevel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "advice")]
pub enum SigLevelAdvice {
    /// Signatures stay optional while the server signs what it published
    /// before; `Required` is for once it is done.
    WaitForBackfill,
    /// The server has signed everything: `Required` in place of the
    /// optional level set now.
    Require,
}

/// What to advise about the `SigLevel` `configured` for the repository `key`
/// signs: nothing unless it is the optional level this writes by default --
/// any other is someone's own choice.
#[must_use]
pub fn siglevel_advice(key: &RepoKey, configured: &str) -> Option<SigLevelAdvice> {
    if configured != DEFAULT_SIGLEVEL {
        return None;
    }
    Some(if key.backfilling {
        SigLevelAdvice::WaitForBackfill
    } else {
        SigLevelAdvice::Require
    })
}

/// On stderr, what `advice` asks of the `[name]` section of the pacman
/// configuration `conf` names.
pub fn print_siglevel_advice(advice: &SigLevelAdvice, name: &str, conf: &str) {
    match advice {
        SigLevelAdvice::WaitForBackfill => eprintln!(
            "The server is still signing packages it published before, so [{name}] keeps              SigLevel = {DEFAULT_SIGLEVEL} for now. Once it is done -- `aurcli repo config`              then prints SigLevel = {SIGNED_SIGLEVEL} -- set SigLevel = {SIGNED_SIGLEVEL} in              the [{name}] section of {conf}."
        ),
        SigLevelAdvice::Require => eprintln!(
            "Everything the server publishes is signed: set SigLevel = {SIGNED_SIGLEVEL} in              the [{name}] section of {conf}, in place of {DEFAULT_SIGLEVEL}."
        ),
    }
}

/// Whether the pacman configuration at `path` already declares `[name]`. A
/// file that is not there declares nothing.
pub fn is_configured(path: &Path, name: &str) -> Result<bool> {
    match fs::read_to_string(path) {
        Ok(conf) => Ok(has_section(&conf, name)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => {
            Err(anyhow::Error::new(e)).with_context(|| format!("failed to read {}", path.display()))
        }
    }
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
pub fn install_stanza(path: &Path, config: &RepoConfig) -> Result<Install> {
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
        return Ok(Install::AlreadyPresent);
    }

    let updated = append_stanza(&existing, &config.block);
    match fs::write(path, &updated) {
        Ok(()) => Ok(Install::Appended),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            append_via_sudo(path, &config.block)?;
            Ok(Install::AppendedWithSudo)
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

/// What `--install` did, and to which file.
#[derive(Debug, Clone, Serialize)]
pub struct Installed<'a> {
    pub path: &'a Path,
    pub outcome: Install,
    /// What became of the repository's key, when it is signed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<KeyTrust>,
    /// What is left to change in the section's `SigLevel`, when it is
    /// signed and still the optional level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub siglevel: Option<SigLevelAdvice>,
}

/// What `--install` did to `pacman.conf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Install {
    /// The section was there already, which is the common case on a second
    /// run.
    AlreadyPresent,
    Appended,
    /// Appended through `sudo`, the file not being writable as is.
    AppendedWithSudo,
}

/// Print the stanza on stdout and the what-to-do-with-it on stderr.
///
/// Split that way on purpose: `aurcli repo config >> pacman.conf` has to
/// append three usable lines and nothing else, so the instructions cannot share
/// the stream with them.
pub fn print_repo_config(config: &RepoConfig) {
    print!("{}", config.block);
    eprintln!();
    if let Some(key) = &config.key {
        print_trust_instructions(key);
        if let Some(advice) = siglevel_advice(key, &config.siglevel) {
            print_siglevel_advice(&advice, &config.name, "pacman.conf");
        }
        eprintln!();
    }
    eprintln!("Add it to /etc/pacman.conf (asks for sudo only if it has to):");
    eprintln!("  aurcli repo config --install");
}

/// What to run so that pacman trusts `key`, on stderr.
pub fn print_trust_instructions(key: &RepoKey) {
    eprintln!(
        "The repository is signed with key {}. pacman checks every signature it finds,",
        key.fingerprint
    );
    eprintln!("so trust that key first:");
    for command in trust_commands(key).iter() {
        eprintln!("  {command}");
    }
}

/// The commands that make pacman trust `key`, as one would type them.
#[must_use]
pub fn trust_commands(key: &RepoKey) -> KeyTrustCommands {
    KeyTrustCommands::new(&key.url, &key.fingerprint)
}

/// What `--install` did about the repository's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyTrust {
    /// Added to pacman's keyring and locally signed.
    Trusted,
    /// Left alone: declined at the prompt, or there was no one to ask.
    NotTrusted,
}

/// Add `armored` -- the key the server served -- to pacman's keyring, and
/// locally sign it by `fingerprint`, as `pacman-key` does: through `sudo`
/// unless already root.
///
/// The signing is by the fingerprint the server reported, so a served key
/// that is not that one is added but never trusted, and this fails.
pub fn trust_key(armored: &str, fingerprint: &str) -> Result<()> {
    // `-` for the pipe: `pacman-key --add` with no file refuses outright.
    let mut add = pacman_key()
        .args(["--add", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    add.stdin
        .take()
        .context("pacman-key stdin was not available")?
        .write_all(armored.as_bytes())
        .context("failed to send the key to pacman-key")?;
    if !add.wait().context("waiting for pacman-key")?.success() {
        bail!("pacman-key --add did not add the repository's key");
    }
    let signed = pacman_key()
        .args(["--lsign-key", fingerprint])
        .status()
        .map_err(spawn_error)?;
    if !signed.success() {
        bail!("pacman-key --lsign-key {fingerprint} failed; is that the key the server served?");
    }
    Ok(())
}

/// `pacman-key`, run as root: directly when we are, else through `sudo`,
/// which prompts on the terminal.
fn pacman_key() -> Command {
    if running_as_root() {
        Command::new("pacman-key")
    } else {
        let mut command = Command::new("sudo");
        command.arg("pacman-key");
        command
    }
}

/// Whether this process runs as root: `/proc/self` belongs to the
/// effective user.
fn running_as_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self").is_ok_and(|meta| meta.uid() == 0)
}

fn spawn_error(error: std::io::Error) -> anyhow::Error {
    if error.kind() == ErrorKind::NotFound {
        anyhow!("cannot run pacman-key (or sudo); trust the key by hand")
    } else {
        anyhow::Error::new(error).context("failed to run pacman-key")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_REPO_NAME, DEFAULT_SIGLEVEL, Install, SIGNED_SIGLEVEL, SigLevelAdvice,
        append_stanza, has_section, install_stanza, is_configured, repo_config, repo_server_url,
        section_siglevel, siglevel_advice, trust_commands,
    };
    use aurcache_common::api::repo::{RepoInfo, SigningInfo};

    /// What a server answers, `listen_port` absent as from
    /// one too old to report it.
    fn published(public_url: &str, listen_port: Option<u16>) -> RepoInfo {
        RepoInfo {
            name: DEFAULT_REPO_NAME.to_string(),
            public_url: public_url.to_string(),
            listen_port,
        }
    }

    fn signing(backfilling: bool) -> SigningInfo {
        SigningInfo {
            fingerprint: "0123456789ABCDEF0123456789ABCDEF01234567".to_string(),
            backfilling,
        }
    }

    #[test]
    fn the_server_name_is_used_unless_explicitly_overridden() {
        let mut info = published("http://localhost:8081", Some(8081));
        info.name = "custom_name".to_string();
        let config =
            repo_config("http://host:8080/api", None, None, None, Some(&info), None).unwrap();
        assert_eq!(config.name, "custom_name");
        assert!(config.block.starts_with("[custom_name]\n"));
        let config = repo_config(
            "http://host:8080/api",
            None,
            Some("override".into()),
            None,
            Some(&info),
            None,
        )
        .unwrap();
        assert_eq!(config.name, "override");
    }

    #[test]
    fn an_invalid_section_name_is_rejected() {
        assert!(
            repo_config(
                "http://host:8080/api",
                None,
                Some("bad]\nServer = bad".into()),
                None,
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn the_repo_is_the_api_host_on_the_repository_port() {
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
        let config =
            repo_config("http://localhost:8080/api", None, None, None, None, None).unwrap();
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

        let config =
            repo_config("http://localhost:8080/api", None, None, None, None, None).unwrap();
        assert_eq!(install_stanza(&path, &config).unwrap(), Install::Appended);
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

        let config =
            repo_config("http://localhost:8080/api", None, None, None, None, None).unwrap();
        assert_eq!(install_stanza(&path, &config).unwrap(), Install::Appended);
        let once = std::fs::read_to_string(&path).unwrap();

        assert_eq!(
            install_stanza(&path, &config).unwrap(),
            Install::AlreadyPresent
        );
        assert_eq!(once, std::fs::read_to_string(&path).unwrap());
        assert_eq!(once.matches("[repo]").count(), 1, "{once}");
    }

    /// A deployment publishing on a non-default port is given that port, not
    /// 8081.
    #[test]
    fn the_servers_port_and_path_are_taken_over_the_default() {
        let config = repo_config(
            "http://192.168.1.5:8080/api",
            None,
            None,
            None,
            Some(&published("http://localhost:9000/arch", Some(8081))),
            None,
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
            Some(&published("https://aurcache.example.com/repo", None)),
            None,
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
            Some(&published("http://localhost:8081", Some(8081))),
            None,
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
            Some(&published("http://localhost:9000", Some(9000))),
            None,
        )
        .unwrap();
        assert!(config.server.contains(":9999/"), "{}", config.server);
    }

    /// A server listening on another port reports it, both in its default
    /// published URL and on its own; a URL that names no port means that one.
    #[test]
    fn the_servers_listen_port_fills_in_a_url_without_one() {
        let config = repo_config(
            "http://192.168.1.5:9080/api",
            None,
            None,
            None,
            Some(&published("http://localhost:9081", Some(9081))),
            None,
        )
        .unwrap();
        assert_eq!(config.server, "http://192.168.1.5:9081/$arch");

        let config = repo_config(
            "http://192.168.1.5:9080/api",
            None,
            None,
            None,
            Some(&published("http://aurcache.example.com/repo", Some(9081))),
            None,
        )
        .unwrap();
        assert_eq!(config.server, "http://aurcache.example.com:9081/repo/$arch");
    }

    /// Without the server's answer the port is the default: a guess, and the
    /// API URL's own port says nothing about the repository's.
    #[test]
    fn without_the_server_the_default_port_is_assumed() {
        let config =
            repo_config("http://192.168.1.5:9080/api", None, None, None, None, None).unwrap();
        assert_eq!(config.server, "http://192.168.1.5:8081/$arch");
    }

    #[test]
    fn a_missing_pacman_conf_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let config =
            repo_config("http://localhost:8080/api", None, None, None, None, None).unwrap();
        let err = install_stanza(&dir.path().join("nope.conf"), &config).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    /// A signed repository gets signatures required, and the key to trust;
    /// one that does not sign keeps them optional and names no key.
    #[test]
    fn a_signed_repository_requires_signatures() {
        let api = "http://192.168.1.5:8080/api";
        let signed = repo_config(api, None, None, None, None, Some(&signing(false))).unwrap();
        assert_eq!(signed.siglevel, SIGNED_SIGLEVEL);
        let key = signed.key.unwrap();
        assert_eq!(key.fingerprint, "0123456789ABCDEF0123456789ABCDEF01234567");
        assert_eq!(key.url, "http://192.168.1.5:8080/api/repo/key");

        let unsigned =
            repo_config(api, None, None, None, Some(&published(api, None)), None).unwrap();
        assert_eq!(unsigned.siglevel, DEFAULT_SIGLEVEL);
        assert!(unsigned.key.is_none());
    }

    /// Until everything published before is signed, requiring signatures
    /// would refuse what is not yet; the key is still to be trusted, for what
    /// is signed already.
    #[test]
    fn a_repository_still_backfilling_keeps_signatures_optional() {
        let config = repo_config(
            "http://localhost:8080/api",
            None,
            None,
            None,
            None,
            Some(&signing(true)),
        )
        .unwrap();
        assert_eq!(config.siglevel, DEFAULT_SIGLEVEL);
        assert!(config.key.unwrap().backfilling);
    }

    #[test]
    fn an_explicit_siglevel_outranks_signing() {
        let config = repo_config(
            "http://localhost:8080/api",
            None,
            None,
            Some("Never".to_string()),
            None,
            Some(&signing(false)),
        )
        .unwrap();
        assert_eq!(config.siglevel, "Never");
    }

    #[test]
    fn the_trust_commands_fetch_and_sign_the_key() {
        let config = repo_config(
            "http://localhost:8080/api",
            None,
            None,
            None,
            None,
            Some(&signing(false)),
        )
        .unwrap();
        let commands = trust_commands(&config.key.unwrap());
        assert_eq!(
            commands.add,
            "curl -fsSL http://localhost:8080/api/repo/key | sudo pacman-key --add -"
        );
        assert_eq!(
            commands.lsign,
            "sudo pacman-key --lsign-key 0123456789ABCDEF0123456789ABCDEF01234567"
        );
    }

    /// What decides whether `--install` may write a stanza naming a key
    /// pacman does not trust: a section that is already there is not written.
    #[test]
    fn a_configured_section_is_seen_and_a_missing_file_declares_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacman.conf");
        assert!(!is_configured(&path, "repo").unwrap());
        std::fs::write(&path, "[options]\n\n[repo]\nServer = x\n").unwrap();
        assert!(is_configured(&path, "repo").unwrap());
        assert!(!is_configured(&path, "other").unwrap());
    }

    /// The `SigLevel` of one section, not of the one before or after it, and
    /// none for a section that sets none.
    #[test]
    fn a_sections_siglevel_is_read_from_that_section() {
        let conf = "[options]\nSigLevel = Required DatabaseOptional\n\n[repo]\n\
                    SigLevel = Optional TrustAll\nServer = x\n\n[other]\nServer = y\n";
        assert_eq!(
            section_siglevel(conf, "repo").as_deref(),
            Some(DEFAULT_SIGLEVEL)
        );
        assert_eq!(section_siglevel(conf, "other"), None);
        assert_eq!(section_siglevel(conf, "missing"), None);
    }

    /// A repository still backfilling keeps signatures optional, with what
    /// to change once it is done; one that is done has the optional level
    /// it was given turned into `Required`. A level someone chose is left
    /// alone.
    #[test]
    fn the_optional_level_is_not_left_for_good() {
        let config = |backfilling| {
            repo_config(
                "http://localhost:8080/api",
                None,
                None,
                None,
                None,
                Some(&signing(backfilling)),
            )
            .unwrap()
        };
        let backfilling = config(true);
        assert_eq!(
            siglevel_advice(backfilling.key.as_ref().unwrap(), &backfilling.siglevel),
            Some(SigLevelAdvice::WaitForBackfill)
        );
        let done = config(false);
        let key = done.key.as_ref().unwrap();
        assert_eq!(
            siglevel_advice(key, DEFAULT_SIGLEVEL),
            Some(SigLevelAdvice::Require)
        );
        assert_eq!(siglevel_advice(key, SIGNED_SIGLEVEL), None);
        assert_eq!(siglevel_advice(key, "Never"), None);
    }
}
