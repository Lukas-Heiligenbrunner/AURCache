//! Build credentials made available inside the build chroot.
//!
//! Some AUR packages fetch their sources over authenticated transports —
//! `unreal-engine` clones `git+ssh://git@github.com/EpicGames/UnrealEngine`,
//! which requires an SSH key belonging to a GitHub account in the Epic Games
//! organisation. That fetch happens inside the chroot during `makepkg`, not on
//! the server, so the key has to reach the build environment.
//!
//! Two modes, in precedence order:
//!
//! 1. `WORKER_GIT_SSH_KEY` names a key — use exactly that one and generate
//!    nothing. This is the zero-touch path: the credential is provisioned with
//!    the machine, so deployment needs no follow-up action.
//! 2. Otherwise the worker generates an ed25519 keypair under `<data_dir>/ssh`
//!    on first start and logs the **public** half. Onboarding then costs one
//!    manual step: add that public key to the GitHub account. This is the
//!    documented default because a public key can only ever be attached to one
//!    GitHub account while an account may hold many — so N workers each holding
//!    their own key is the shape GitHub wants, not a workaround, and no private
//!    key is ever copied between machines.
//!
//! Generation happens *only* when no key was provided: a second, unused keypair
//! would make "which key is actually in use?" ambiguous at exactly the moment
//! someone is debugging an authentication failure.

use anyhow::{Context, Result};
use aurcache_common::shell::quote_argument;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::build::BindMount;
use crate::config::Config;

/// Directory the staged `known_hosts` lives in — on the worker only.
///
/// `makechrootpkg` downloads sources **outside** the chroot: its
/// `download_sources()` runs `makepkg --verifysource` as the build user on the
/// worker, using the chroot's `makepkg.conf` but not its filesystem. So this is
/// where what ssh reads must be readable.
///
/// It is never bind-mounted into the chroot. Everything a PKGBUILD executes —
/// `prepare`, `build`, `package` — runs in there, and a hostile or merely
/// careless one could read and exfiltrate anything within reach.
///
/// The path is stable across jobs: [`stage_for_job`] overwrites what is there
/// for each one, and the `known_hosts` path it puts in `GIT_SSH_COMMAND` stays
/// the same. The key is never here: builds use it through the agent.
pub fn secrets_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("secrets")
}
/// Filename of the key the worker generates.
pub const KEY_FILE: &str = "id_ed25519";
/// `known_hosts` filename within that directory.
pub const KNOWN_HOSTS_FILE: &str = "known_hosts";

/// Where the worker generates its own key when none is provided.
pub fn generated_key(data_dir: &Path) -> PathBuf {
    data_dir.join("ssh").join(KEY_FILE)
}

/// A credential staged for a build: the agent holding the key, and the
/// `GIT_SSH_COMMAND` that uses it.
///
/// No key path is carried. The agent loaded the key from where it was
/// configured when the worker started (see [`crate::agent::start`]), and the
/// command must never name it: the build user cannot read the file (see
/// [`git_ssh_command`]).
#[derive(Debug, Clone)]
pub struct StagedCredential {
    pub git_ssh_command: String,
    /// The agent's socket, which builds reach on the same path inside the
    /// chroot and out.
    pub agent_socket: PathBuf,
}

/// Which SSH key a build should use, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// Explicitly configured by the operator; never generated or overwritten.
    Provided(PathBuf),
    /// Worker-generated fallback, created on first start if absent.
    Generated(PathBuf),
}

impl KeySource {
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Provided(p) | Self::Generated(p) => p,
        }
    }
}

/// Decide which key this worker builds with.
#[must_use]
pub fn resolve(cfg: &Config) -> KeySource {
    match &cfg.git_ssh_key {
        Some(path) => KeySource::Provided(path.clone()),
        None => KeySource::Generated(generated_key(&cfg.data_dir)),
    }
}

/// Public-key path for a private key, by OpenSSH's `.pub` convention.
#[must_use]
pub fn public_key_path(private: &Path) -> PathBuf {
    let mut name = private.as_os_str().to_os_string();
    name.push(".pub");
    PathBuf::from(name)
}

/// Ensure a generated key exists, creating it on first start.
///
/// A [`KeySource::Provided`] key is never touched — if it is missing that is an
/// operator error, and silently generating a different key in its place would
/// produce authentication failures that look like a server-side problem.
///
/// Returns the public key text when one is available, for logging.
pub async fn ensure(source: &KeySource) -> Result<Option<String>> {
    let path = source.path();
    match source {
        KeySource::Provided(_) => {
            if !path.exists() {
                anyhow::bail!(
                    "WORKER_GIT_SSH_KEY points at {}, which does not exist",
                    path.display()
                );
            }
            Ok(None)
        }
        KeySource::Generated(_) => {
            if !path.exists() {
                generate(path).await?;
            }
            Ok(std::fs::read_to_string(public_key_path(path)).ok())
        }
    }
}

/// Generate an ed25519 keypair with `ssh-keygen`.
///
/// Shelling out rather than pulling in an SSH key library keeps the format
/// question with the tool that defines it, and matches how the rest of this
/// crate drives `devtools`.
async fn generate(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating key dir {}", dir.display()))?;
    }
    let status = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "aurcache-worker", "-f"])
        .arg(path)
        .status()
        .await
        .context("running ssh-keygen (is openssh installed?)")?;
    anyhow::ensure!(status.success(), "ssh-keygen failed with {status}");
    Ok(())
}

/// The `GIT_SSH_COMMAND` a build should use.
///
/// The key is **not** named here, and `IdentitiesOnly` is deliberately absent:
/// authentication goes through the ssh-agent the worker runs, reached via
/// `SSH_AUTH_SOCK`. The build user cannot read the key file at all — it belongs
/// to the worker's user and is listed in the sandbox's protected paths — so a
/// `-i` pointing at it would simply fail.
///
/// This is what makes the credential survive the uid split: the agent lets a
/// build *use* the key without ever being able to read it. A hostile PKGBUILD
/// can still ask the agent to authenticate during its own build (agent
/// hijacking is inherent and accepted), but it cannot obtain the key material,
/// so nothing it steals outlives the build.
#[must_use]
pub fn git_ssh_command(known_hosts: Option<&str>) -> String {
    let mut cmd = String::from("ssh");
    match known_hosts {
        Some(path) => {
            // Quoted for the shell git runs this through: an unquoted path
            // with a space would split into two ssh arguments.
            cmd.push_str(" -o UserKnownHostsFile=");
            cmd.push_str(&quote_argument(path));
        }
        // Without a known_hosts file ssh would prompt, which hangs a
        // non-interactive build forever; accept-new records on first contact
        // and still fails on a *changed* key.
        None => cmd.push_str(" -o StrictHostKeyChecking=accept-new"),
    }
    cmd
}

/// Append worker-local exports to the server-rendered `makepkg.conf`.
///
/// `makepkg.conf` is sourced by `makepkg`, so an `export` here reaches `git`
/// without `makechrootpkg` having to forward environment variables. Keeping it
/// worker-side means credentials never enter the server's job configuration.
///
/// `SSH_AUTH_SOCK` is **set here rather than inherited**, and that is what makes
/// the credential work inside the chroot as well as outside.
///
/// `makechrootpkg` preserves `SSH_AUTH_SOCK` for `download_sources()`, which
/// runs on the worker -- but the build proper runs through a generated
/// `chrootbuild` script under `arch-nspawn`, which carries no environment in at
/// all. So a guard on the variable already being set holds only outside, and a
/// PKGBUILD that clones in `prepare()` -- as `unreal-engine` must, makepkg
/// having no shallow-clone support -- saw no agent.
///
/// Naming the socket explicitly works in both: the same path is bound into the
/// chroot, so the guard is on the socket *existing* rather than on an
/// environment this file cannot count on. Where there is no agent the guard
/// fails and git behaves as if AURCache had never touched the config.
#[must_use]
pub fn augment_makepkg_conf(base: &str, credential: Option<&StagedCredential>) -> String {
    let Some(cred) = credential else {
        return base.to_string();
    };
    let mut out = base.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# Added by aurcache-worker: build credentials (see credentials.rs).\n");
    out.push_str("# The socket is named rather than inherited: arch-nspawn carries no\n");
    out.push_str("# environment into the chroot, so a build that fetches there would\n");
    out.push_str("# otherwise see no agent. Guarded on the socket existing, so a worker\n");
    out.push_str("# without a credential is unaffected.\n");
    out.push_str(&format!(
        "if [ -S {sock} ]; then\n    \
         export SSH_AUTH_SOCK={sock}\n    \
         export GIT_SSH_COMMAND={cmd}\nfi\n",
        sock = quote_argument(&cred.agent_socket.display().to_string()),
        cmd = quote_argument(&cred.git_ssh_command)
    ));
    out
}

/// Parse `WORKER_BIND_MOUNTS` (`host:chroot,host:chroot`).
///
/// Entries without exactly one `:` are dropped rather than guessed at: a
/// half-parsed bind mount would silently expose the wrong path into a build.
/// Each one dropped is reported, since a mount the operator asked for and
/// the build never sees is otherwise only discovered as a failing build.
#[must_use]
pub fn parse_bind_mounts(raw: &str) -> Vec<BindMount> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            let pair = entry
                .split_once(':')
                .map(|(host, chroot)| (host.trim(), chroot.trim()))
                .filter(|(host, chroot)| {
                    !host.is_empty() && !chroot.is_empty() && !chroot.contains(':')
                });
            if pair.is_none() {
                tracing::warn!(
                    "ignoring WORKER_BIND_MOUNTS entry {entry:?} (expected host:chroot)"
                );
            }
            pair.map(|(host, chroot)| BindMount {
                host: PathBuf::from(host),
                chroot: PathBuf::from(chroot),
            })
        })
        .collect()
}

/// Stage the build credentials for one job into `dir`, returning the
/// `GIT_SSH_COMMAND` to export; `None` when there is no credential to use.
///
/// A credential is the ssh-agent at `agent_socket`, which loaded the key when
/// the worker started: without one, there is none, whatever key is on disk.
/// So replacing the key takes effect when the worker restarts, not at the
/// next build.
///
/// What is staged is `known_hosts`, **copied** rather than bind-mounted from
/// wherever the operator put it: a docker-secret or root-owned mount would not
/// be readable by the unprivileged build user. Read per job, so a replaced
/// file takes effect on the next build. The destination is shared by every
/// job (see [`secrets_dir`]) and simply overwritten each time.
pub fn stage_for_job(
    known_hosts: Option<&Path>,
    dir: &Path,
    agent_socket: Option<&Path>,
) -> Result<Option<StagedCredential>> {
    let Some(agent_socket) = agent_socket else {
        return Ok(None);
    };
    let known_hosts = match known_hosts {
        Some(src) if src.exists() => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating secrets dir {}", dir.display()))?;
            let staged = dir.join(KNOWN_HOSTS_FILE);
            std::fs::copy(src, &staged).context("copying known_hosts")?;
            Some(staged.display().to_string())
        }
        _ => None,
    };

    Ok(Some(StagedCredential {
        git_ssh_command: git_ssh_command(known_hosts.as_deref()),
        agent_socket: agent_socket.to_path_buf(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(key: Option<&str>, data_dir: &str) -> Config {
        let mut cfg = Config::from_process_env();
        cfg.git_ssh_key = key.map(PathBuf::from);
        cfg.data_dir = PathBuf::from(data_dir);
        cfg.ssh_known_hosts = None;
        cfg
    }

    fn credential(socket: &str) -> StagedCredential {
        StagedCredential {
            git_ssh_command: "ssh -o StrictHostKeyChecking=accept-new".to_string(),
            agent_socket: PathBuf::from(socket),
        }
    }

    #[test]
    fn an_explicit_key_takes_precedence_and_is_never_generated() {
        let source = resolve(&cfg_with(Some("/run/secrets/epic"), "/var/lib/w"));
        assert_eq!(
            source,
            KeySource::Provided(PathBuf::from("/run/secrets/epic"))
        );
    }

    #[test]
    fn without_an_explicit_key_one_is_generated_under_data_dir() {
        let source = resolve(&cfg_with(None, "/var/lib/w"));
        assert_eq!(
            source,
            KeySource::Generated(PathBuf::from("/var/lib/w/ssh/id_ed25519"))
        );
    }

    /// A missing *provided* key must be an error, not a silent fallback to a
    /// generated one — the resulting auth failures would look server-side.
    #[tokio::test]
    async fn a_missing_provided_key_is_an_error() {
        let source = KeySource::Provided(PathBuf::from("/nonexistent/key"));
        assert!(ensure(&source).await.is_err());
    }

    #[test]
    fn public_key_path_follows_openssh_convention() {
        assert_eq!(
            public_key_path(Path::new("/k/id_ed25519")),
            PathBuf::from("/k/id_ed25519.pub")
        );
    }

    /// Authentication goes through the agent, so the command must name no key
    /// and must not set `IdentitiesOnly`, which would suppress agent identities
    /// and break authenticated fetches outright.
    #[test]
    fn git_ssh_command_uses_the_agent_and_sets_host_policy() {
        let with_hosts = git_ssh_command(Some("/staged/known_hosts"));
        assert!(!with_hosts.contains("-i "), "must not name a key file");
        assert!(!with_hosts.contains("IdentitiesOnly"));
        assert!(with_hosts.contains("UserKnownHostsFile=/staged/known_hosts"));
        // No known_hosts must not leave ssh prompting, which would hang a build.
        let without = git_ssh_command(None);
        assert!(without.contains("StrictHostKeyChecking=accept-new"));
    }

    #[test]
    fn makepkg_conf_gains_the_export_only_when_a_credential_exists() {
        let base = "PKGDEST=/output";
        assert_eq!(augment_makepkg_conf(base, None), base);

        let cred = credential("/run/a/agent.sock");
        let augmented = augment_makepkg_conf(base, Some(&cred));
        assert!(augmented.starts_with("PKGDEST=/output\n"));
        assert!(
            augmented.contains("export GIT_SSH_COMMAND='ssh -o StrictHostKeyChecking=accept-new'")
        );
    }

    /// The socket is named, not inherited, and the export is guarded on it
    /// existing.
    ///
    /// `arch-nspawn` carries no environment into the chroot, so a guard on
    /// `$SSH_AUTH_SOCK` already being set held only outside it -- and a
    /// PKGBUILD that clones in `prepare()` therefore saw no agent at all. The
    /// same path is bound into the chroot, so naming it works in both places
    /// while a worker without an agent is still left alone.
    #[test]
    fn the_export_names_the_socket_and_is_guarded_on_it() {
        let cred = credential("/var/lib/aurcache-worker/agent/agent.sock");
        let conf = augment_makepkg_conf("PKGDEST=/output", Some(&cred));

        assert!(conf.contains("export SSH_AUTH_SOCK=/var/lib/aurcache-worker/agent/agent.sock"));
        assert!(conf.trim_end().ends_with("fi"));
        // The guard must wrap both exports, not sit beside them.
        let guard = conf
            .find("if [ -S /var/lib/aurcache-worker/agent/agent.sock ]")
            .expect("guard names the socket");
        let sock = conf.find("export SSH_AUTH_SOCK").expect("socket export");
        let git = conf.find("export GIT_SSH_COMMAND").expect("git export");
        assert!(
            guard < sock && guard < git,
            "exports must be inside the guard"
        );
        // The key path must never leak into a config the chroot also reads.
        assert!(!conf.contains("/staged/id_ed25519"));
    }

    /// Quoting is load-bearing for operator-controlled paths (a data dir with
    /// a space, `$` or backticks), so it round-trips through a real shell.
    #[test]
    fn hostile_paths_survive_a_real_shell_as_one_value() {
        for hostile in [
            "/data/my dir/agent.sock",
            "/data/$x/`id`/agent.sock",
            "/it's/agent.sock",
        ] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "export PROBE={}; printf %s \"$PROBE\"",
                    quote_argument(hostile)
                ))
                .output()
                .expect("sh should run");
            assert!(out.status.success(), "shell rejected {hostile:?}");
            assert_eq!(String::from_utf8_lossy(&out.stdout), hostile);
        }
    }

    /// The known-hosts path travels inside `GIT_SSH_COMMAND`, which git
    /// splits on spaces: it must arrive there as one shell word.
    #[test]
    fn a_spaced_known_hosts_path_stays_one_ssh_argument() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "printf '<%s>' {}",
                git_ssh_command(Some("/my dir/known hosts"))
            ))
            .output()
            .expect("sh should run");
        assert!(out.status.success());
        // Three words, path intact: the shell consumed the quotes while
        // splitting, which is exactly what protects the spaces.
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "<ssh><-o><UserKnownHostsFile=/my dir/known hosts>"
        );
    }

    /// No agent means no credential, and no exports at all, so a worker
    /// without one builds exactly as it would if none of this existed --
    /// whatever key lies on disk.
    #[test]
    fn without_an_agent_there_is_no_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let hosts = tmp.path().join("hosts");
        std::fs::write(&hosts, "github.com ssh-ed25519 AAAA").unwrap();
        let staged = stage_for_job(Some(&hosts), &secrets_dir(tmp.path()), None).unwrap();
        assert!(staged.is_none());
        assert_eq!(
            augment_makepkg_conf("PKGDEST=/output", staged.as_ref()),
            "PKGDEST=/output"
        );
    }

    #[test]
    fn bind_mounts_parse_pairs_and_drop_malformed_entries() {
        let parsed = parse_bind_mounts("/host/a:/chroot/a, /host/b:/chroot/b");
        assert_eq!(
            parsed,
            vec![
                BindMount {
                    host: PathBuf::from("/host/a"),
                    chroot: PathBuf::from("/chroot/a"),
                },
                BindMount {
                    host: PathBuf::from("/host/b"),
                    chroot: PathBuf::from("/chroot/b"),
                },
            ]
        );
        // Guessing at a half-specified mount could expose the wrong path.
        assert!(parse_bind_mounts("/host/a").is_empty());
        assert!(parse_bind_mounts(":/chroot").is_empty());
        assert!(parse_bind_mounts("/host:").is_empty());
        assert!(parse_bind_mounts("/a:/b:/c").is_empty());
        assert!(parse_bind_mounts("").is_empty());
    }

    /// The key stays where the agent loaded it from: nothing copies it into
    /// the secrets directory, where nothing would read it. `known_hosts` is
    /// staged, and named in the command.
    #[test]
    fn staging_copies_known_hosts_and_no_key() {
        let tmp = tempfile::tempdir().unwrap();
        let hosts = tmp.path().join("src-hosts");
        std::fs::write(&hosts, "github.com ssh-ed25519 AAAA").unwrap();

        let dest = secrets_dir(tmp.path());
        let socket = tmp.path().join("agent.sock");
        let cred = stage_for_job(Some(&hosts), &dest, Some(&socket))
            .unwrap()
            .expect("staged");

        assert_eq!(cred.agent_socket, socket);
        let staged = dest.join(KNOWN_HOSTS_FILE);
        assert!(cred.git_ssh_command.contains(&staged.display().to_string()));
        assert!(!cred.git_ssh_command.contains("-i "));
        assert!(!dest.join(KEY_FILE).exists(), "no key is staged");
    }
}
