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
use aurcache_common::fs::write_public;
use aurcache_common::shell::quote_argument;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::build::{Access, BindMount};
use crate::config::Config;

/// Directory the staged `known_hosts` lives in.
///
/// ssh reads it as the build user in two places: outside the chroot, where
/// `makechrootpkg`'s `download_sources()` runs `makepkg --verifysource`, and
/// inside it, for a PKGBUILD that clones in `prepare()` -- it is bound there,
/// read-only, at the same path (see `job::prepare_bind_mounts`). So
/// [`stage_for_job`] gives it modes that let the build user read it and only
/// the worker write it.
///
/// Not a secret, and so not under the paths `aurcache-sandbox` keeps from the
/// code outside the chroot (`packaging/sandbox-protected`): a `known_hosts`
/// there is one ssh cannot open, which makes every host unknown and every SSH
/// fetch fail at host-key verification. The key is never here: builds use it
/// through the agent.
///
/// The path is stable across jobs: [`stage_for_job`] replaces what is there
/// for each one, and the `known_hosts` path it puts in `GIT_SSH_COMMAND` stays
/// the same.
pub fn known_hosts_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("ssh-known-hosts")
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
    /// The staged `known_hosts` that `git_ssh_command` names, which builds
    /// read on the same path inside the chroot and out; `None` when none is
    /// configured.
    pub known_hosts: Option<PathBuf>,
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

/// What ssh is told beside a configured `known_hosts`, so that no other source
/// of host keys can admit a host the file does not name: no other
/// `known_hosts` (the system's `/etc/ssh/ssh_known_hosts` included), no
/// `KnownHostsCommand`, no key from DNS, and no host accepted on first use.
///
/// Options on ssh's command line win over every `ssh_config`, the system's and
/// the build user's, so none of them can turn any of this back on.
const PINNED: [&str; 4] = [
    "GlobalKnownHostsFile=/dev/null",
    "KnownHostsCommand=none",
    "VerifyHostKeyDNS=no",
    "StrictHostKeyChecking=yes",
];

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
///
/// With a `known_hosts`, that file is the only say in which hosts are trusted
/// ([`PINNED`]).
#[must_use]
pub fn git_ssh_command(known_hosts: Option<&Path>) -> String {
    let mut cmd = String::from("ssh");
    match known_hosts {
        Some(path) => {
            // Quoted for the shell git runs this through: an unquoted path
            // with a space would split into two ssh arguments.
            cmd.push_str(" -o UserKnownHostsFile=");
            cmd.push_str(&quote_argument(&path.display().to_string()));
            for option in PINNED {
                cmd.push_str(" -o ");
                cmd.push_str(option);
            }
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
///
/// Each is bound read-write: the variable has no way to ask for less.
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
                access: Access::ReadWrite,
            })
        })
        .collect()
}

/// A key `WORKER_GIT_SSH_KEYS` names: the key at `path`, offered to the
/// builds whose package's `build_ssh_key` setting is `name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedKey {
    pub name: String,
    pub path: PathBuf,
}

/// Parse `WORKER_GIT_SSH_KEYS` (`name:path,name:path`).
///
/// An entry is dropped, and reported, when its name is not a key name (see
/// [`aurcache_common::ssh_key::validate_name`]), when it is `default` -- the
/// key `WORKER_GIT_SSH_KEY` names, or the generated one, which this cannot
/// redefine -- when an earlier entry has the name already, or when it has no
/// path. A key the operator meant and the worker does not hold is otherwise
/// only discovered as a build that fails at its fetch.
#[must_use]
pub fn parse_named_keys(raw: &str) -> Vec<NamedKey> {
    let mut keys: Vec<NamedKey> = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let Some((name, path)) = entry
            .split_once(':')
            .map(|(name, path)| (name.trim(), path.trim()))
            .filter(|(_, path)| !path.is_empty())
        else {
            tracing::warn!("ignoring WORKER_GIT_SSH_KEYS entry {entry:?} (expected name:path)");
            continue;
        };
        let refusal = if let Err(e) = aurcache_common::ssh_key::validate_name(name) {
            Some(e)
        } else if name == aurcache_common::ssh_key::DEFAULT {
            Some("`default` is the key WORKER_GIT_SSH_KEY names, or the generated one".to_string())
        } else if keys.iter().any(|key| key.name == name) {
            Some(format!("an earlier entry names {name:?} already"))
        } else {
            None
        };
        if let Some(why) = refusal {
            tracing::warn!("ignoring WORKER_GIT_SSH_KEYS entry {entry:?}: {why}");
            continue;
        }
        keys.push(NamedKey {
            name: name.to_string(),
            path: PathBuf::from(path),
        });
    }
    keys
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
/// job (see [`known_hosts_dir`]) and replaced each time.
///
/// A configured `known_hosts` that cannot be read is an error, which fails the
/// job: the operator asked for the hosts it names and no others, and staging
/// none would leave ssh accepting any host on first use.
pub fn stage_for_job(
    known_hosts: Option<&Path>,
    dir: &Path,
    agent_socket: Option<&Path>,
) -> Result<Option<StagedCredential>> {
    let Some(agent_socket) = agent_socket else {
        return Ok(None);
    };
    let known_hosts = known_hosts
        .map(|src| stage_known_hosts(src, dir))
        .transpose()?;

    Ok(Some(StagedCredential {
        git_ssh_command: git_ssh_command(known_hosts.as_deref()),
        agent_socket: agent_socket.to_path_buf(),
        known_hosts,
    }))
}

/// Copy `src` into `dir` as `known_hosts`, readable by every user and
/// writable by the worker alone, and return where it went.
///
/// Written beside the previous copy and renamed over it, never written
/// through it ([`write_public`]):
/// - whatever is at the path is replaced, not followed, whatever the bind
///   keeps out (see `job::known_hosts_bind`): a symlink planted there -- to
///   the worker's mTLS key, say -- gives way to the copy, and what it pointed
///   at is left as it was;
/// - every job reads this one file, and another job's ssh may have it open:
///   that ssh reads the previous copy whole, and one starting after the
///   rename reads this copy whole. Neither ever sees a file being truncated
///   and rewritten under it.
///
/// A directory at the path fails the job instead: the worker never makes one
/// there, so it is not the worker's to remove.
///
/// The modes are the copy's own rather than the source's, so a `0600` secret
/// mount still stages as a file the build user can read.
fn stage_known_hosts(src: &Path, dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let hosts = std::fs::read(src).with_context(|| {
        format!(
            "WORKER_SSH_KNOWN_HOSTS names {}, which cannot be read; refusing to \
             build without the host keys it pins",
            src.display()
        )
    })?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("setting the mode of {}", dir.display()))?;
    let staged = dir.join(KNOWN_HOSTS_FILE);
    match write_public(&staged, &hosts) {
        Ok(()) => Ok(staged),
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => anyhow::bail!(
            "{} is a directory, where the worker stages known_hosts; remove it",
            staged.display()
        ),
        Err(e) => Err(e).with_context(|| format!("writing {}", staged.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, path: &str) -> NamedKey {
        NamedKey {
            name: name.to_string(),
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn named_keys_are_name_path_pairs() {
        assert_eq!(
            parse_named_keys(
                "gitlab:/etc/aurcache-worker/gitlab_ed25519, epic : /run/secrets/epic,"
            ),
            vec![
                named("gitlab", "/etc/aurcache-worker/gitlab_ed25519"),
                named("epic", "/run/secrets/epic"),
            ]
        );
        assert_eq!(parse_named_keys(""), Vec::new());
    }

    /// Only the first `:` separates, so a path may hold one.
    #[test]
    fn a_named_keys_path_may_hold_a_colon() {
        assert_eq!(
            parse_named_keys("gitlab:/keys/a:b"),
            vec![named("gitlab", "/keys/a:b")]
        );
    }

    /// What is no key name, `default`, a name given twice and an entry
    /// without a path are dropped; the rest are kept.
    #[test]
    fn a_named_key_that_cannot_be_one_is_dropped() {
        assert_eq!(
            parse_named_keys(
                "default:/k/d,Gitlab:/k/g,../x:/k/x,gitlab,epic:,gitlab:/k/1,gitlab:/k/2"
            ),
            vec![named("gitlab", "/k/1")]
        );
    }

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
            known_hosts: None,
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
        let with_hosts = git_ssh_command(Some(Path::new("/staged/known_hosts")));
        assert!(!with_hosts.contains("-i "), "must not name a key file");
        assert!(!with_hosts.contains("IdentitiesOnly"));
        assert!(with_hosts.contains("UserKnownHostsFile=/staged/known_hosts"));
        // The configured file is the only one trusted, and a host it does not
        // name is refused rather than accepted.
        for option in [
            "-o GlobalKnownHostsFile=/dev/null",
            "-o KnownHostsCommand=none",
            "-o VerifyHostKeyDNS=no",
            "-o StrictHostKeyChecking=yes",
        ] {
            assert!(with_hosts.contains(option), "{with_hosts}");
        }
        assert!(!with_hosts.contains("accept-new"), "{with_hosts}");
        // No known_hosts must not leave ssh prompting, which would hang a build.
        let without = git_ssh_command(None);
        assert!(without.contains("StrictHostKeyChecking=accept-new"));
    }

    /// The pin holds against an `ssh_config` that trusts more: another
    /// `known_hosts`, a `KnownHostsCommand`, DNS, or any host on first use.
    /// Asked by `ssh -G`, which prints the configuration ssh would connect
    /// with and connects to nothing.
    #[test]
    fn a_pinned_known_hosts_wins_over_any_ssh_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("ssh_config");
        std::fs::write(
            &config,
            "Host *\n  StrictHostKeyChecking no\n  UserKnownHostsFile /elsewhere\n  \
             GlobalKnownHostsFile /etc/ssh/ssh_known_hosts\n  \
             KnownHostsCommand /bin/cat /elsewhere\n  VerifyHostKeyDNS yes\n",
        )
        .unwrap();
        let pinned = tmp.path().join(KNOWN_HOSTS_FILE);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "{} -F {} -G example.invalid",
                git_ssh_command(Some(&pinned)),
                quote_argument(&config.display().to_string())
            ))
            .output()
            .expect("sh should run");
        assert!(
            out.status.success(),
            "ssh -G failed (is openssh installed?): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let effective = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = effective.lines().collect();
        for expected in [
            format!("userknownhostsfile {}", pinned.display()),
            "globalknownhostsfile /dev/null".to_string(),
            "stricthostkeychecking true".to_string(),
            "verifyhostkeydns false".to_string(),
        ] {
            assert!(
                lines.contains(&expected.as_str()),
                "{expected}: {effective}"
            );
        }
        assert!(
            !lines
                .iter()
                .any(|line| line.starts_with("knownhostscommand")),
            "{effective}"
        );
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
        let cred = credential("/var/lib/aurcache-worker/agent/default/agent.sock");
        let conf = augment_makepkg_conf("PKGDEST=/output", Some(&cred));

        assert!(
            conf.contains("export SSH_AUTH_SOCK=/var/lib/aurcache-worker/agent/default/agent.sock")
        );
        assert!(conf.trim_end().ends_with("fi"));
        // The guard must wrap both exports, not sit beside them.
        let guard = conf
            .find("if [ -S /var/lib/aurcache-worker/agent/default/agent.sock ]")
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
                git_ssh_command(Some(Path::new("/my dir/known hosts")))
            ))
            .output()
            .expect("sh should run");
        assert!(out.status.success());
        // The path one word, intact: the shell consumed the quotes while
        // splitting, which is exactly what protects the spaces.
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "<ssh><-o><UserKnownHostsFile=/my dir/known hosts>\
             <-o><GlobalKnownHostsFile=/dev/null><-o><KnownHostsCommand=none>\
             <-o><VerifyHostKeyDNS=no><-o><StrictHostKeyChecking=yes>"
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
        let staged = stage_for_job(Some(&hosts), &known_hosts_dir(tmp.path()), None).unwrap();
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
                    access: Access::ReadWrite,
                },
                BindMount {
                    host: PathBuf::from("/host/b"),
                    chroot: PathBuf::from("/chroot/b"),
                    access: Access::ReadWrite,
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

    /// The key stays where the agent loaded it from: nothing copies it beside
    /// `known_hosts`. `known_hosts` is staged, and named in the command.
    #[test]
    fn staging_copies_known_hosts_and_no_key() {
        let tmp = tempfile::tempdir().unwrap();
        let hosts = tmp.path().join("src-hosts");
        std::fs::write(&hosts, "github.com ssh-ed25519 AAAA").unwrap();

        let dest = known_hosts_dir(tmp.path());
        let socket = tmp.path().join("agent.sock");
        let cred = stage_for_job(Some(&hosts), &dest, Some(&socket))
            .unwrap()
            .expect("staged");

        assert_eq!(cred.agent_socket, socket);
        let staged = dest.join(KNOWN_HOSTS_FILE);
        assert_eq!(cred.known_hosts.as_ref(), Some(&staged));
        assert!(cred.git_ssh_command.contains(&staged.display().to_string()));
        assert!(!cred.git_ssh_command.contains("-i "));
        assert!(!dest.join(KEY_FILE).exists(), "no key is staged");
    }

    /// Staged readable by every user and writable by the worker alone,
    /// whatever modes the source had: a `0600` secret mount copied as it is
    /// would be one the build user's ssh cannot open.
    #[test]
    fn staged_known_hosts_is_readable_by_the_build_user_only_to_read() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let hosts = tmp.path().join("src-hosts");
        std::fs::write(&hosts, "github.com ssh-ed25519 AAAA").unwrap();
        std::fs::set_permissions(&hosts, std::fs::Permissions::from_mode(0o600)).unwrap();

        let dest = known_hosts_dir(tmp.path());
        stage_for_job(Some(&hosts), &dest, Some(&tmp.path().join("agent.sock"))).unwrap();

        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dest.join(KNOWN_HOSTS_FILE)), 0o644);
        assert_eq!(mode(&dest), 0o755);
    }

    /// A configured `known_hosts` that cannot be read refuses the job, naming
    /// the setting and the file, rather than building with no pinned hosts --
    /// which would accept any host on first use. Nothing is staged.
    #[test]
    fn a_configured_known_hosts_that_cannot_be_read_refuses_the_job() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-known_hosts");
        let dest = known_hosts_dir(tmp.path());

        let error = stage_for_job(Some(&missing), &dest, Some(&tmp.path().join("agent.sock")))
            .expect_err("an unreadable known_hosts must refuse the job");

        let message = format!("{error:#}");
        assert!(message.contains("WORKER_SSH_KNOWN_HOSTS"), "{message}");
        assert!(
            message.contains(&missing.display().to_string()),
            "{message}"
        );
        assert!(!dest.join(KNOWN_HOSTS_FILE).exists());
    }

    /// A directory where the copy goes fails the job, naming it, and stays
    /// where it is.
    #[test]
    fn a_directory_where_known_hosts_goes_fails_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let hosts = tmp.path().join("src-hosts");
        std::fs::write(&hosts, "github.com ssh-ed25519 AAAA").unwrap();
        let dest = known_hosts_dir(tmp.path());
        let planted = dest.join(KNOWN_HOSTS_FILE);
        std::fs::create_dir_all(planted.join("inside")).unwrap();

        let error = stage_for_job(Some(&hosts), &dest, Some(&tmp.path().join("agent.sock")))
            .expect_err("a directory is not replaced");

        assert_eq!(
            error.to_string(),
            format!(
                "{} is a directory, where the worker stages known_hosts; remove it",
                planted.display()
            )
        );
        assert!(planted.join("inside").is_dir());
    }

    /// The sandbox keeps the paths in its policy from the PKGBUILD code that
    /// runs outside the chroot -- ssh included -- so `known_hosts` must be
    /// staged outside all of them, or every host is unknown there.
    #[test]
    fn known_hosts_is_staged_where_the_sandbox_lets_ssh_read_it() {
        let policy = include_str!("../../../packaging/sandbox-protected");
        let staged = known_hosts_dir(Path::new("/var/lib/aurcache-worker")).join(KNOWN_HOSTS_FILE);
        for line in policy
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            assert!(
                !staged.starts_with(line),
                "{} is under {line}, which the sandbox protects",
                staged.display()
            );
        }
    }
}
