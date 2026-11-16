//! An SSH credential the build can use and cannot read.
//!
//! The key is a file only root can open. Root reads it, starts an
//! `ssh-agent` as the build user (it serves only its own uid), and hands it
//! the key through `ssh-add -` on a pipe, so the key is never a file the
//! build user could open. The build then authenticates through the agent's
//! socket, and an agent does not give back what it holds.

use super::children;
use super::output::Output;
use super::run::{self, Keep};
use super::user::BuildUser;
use aurcache_build_agent::layout::Scratch;
use aurcache_build_agent::spec::Credential;
use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The hosts the build's ssh trusts, where the build user can read them.
const KNOWN_HOSTS: &str = "known_hosts";

/// How long `ssh-agent` has to open its socket, and `ssh-add` to load a key.
const PATIENCE: Duration = Duration::from_secs(30);

/// What the build's `makepkg.conf` needs to know about a loaded credential.
pub struct Loaded {
    pub socket: PathBuf,
    pub known_hosts: Option<PathBuf>,
}

/// Install OpenSSH where the image has none. As root, before the build user
/// exists.
pub fn provide(output: &Output) -> Result<(), String> {
    let mut pacman = Command::new("pacman");
    pacman.args([
        "-S",
        "--needed",
        "--noconfirm",
        "--noprogressbar",
        "--color",
        "never",
        "openssh",
    ]);
    let status = run::logged(output, &mut pacman)
        .map_err(|e| format!("pacman could not be started: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "installing openssh: pacman {}",
            run::describe(status)
        ))
    }
}

/// Start the build user's `ssh-agent` and load the credential into it.
pub fn load(
    credential: &Credential,
    root: &Path,
    scratch: &Scratch,
    user: &BuildUser,
    output: &Output,
) -> Result<Loaded, String> {
    let known_hosts = credential
        .known_hosts
        .as_deref()
        .map(|hosts| publish(hosts, &root.join(KNOWN_HOSTS)))
        .transpose()?;
    let mut key = std::fs::read(&credential.key)
        .map_err(|e| format!("reading {}: {e}", credential.key.display()))?;
    let socket = scratch.ssh_socket();
    let loaded = serve(&socket, user, output).and_then(|()| add(&key, &socket, user));
    key.fill(0);
    loaded.map(|()| Loaded {
        socket,
        known_hosts,
    })
}

/// Copy the known hosts to a file of root's that anyone may read.
fn publish(hosts: &Path, to: &Path) -> Result<PathBuf, String> {
    std::fs::copy(hosts, to)
        .and_then(|_| std::fs::set_permissions(to, Permissions::from_mode(0o644)))
        .map_err(|e| format!("reading {}: {e}", hosts.display()))?;
    Ok(to.to_path_buf())
}

/// Start `ssh-agent` in the foreground, as a child that lives until the
/// build is ended, and wait for its socket.
fn serve(socket: &Path, user: &BuildUser, output: &Output) -> Result<(), String> {
    let mut agent = user.command("ssh-agent");
    agent.arg("-D").arg("-a").arg(socket);
    // On stdout it prints the shell commands that name its socket, which
    // nothing here reads.
    agent
        .stdout(Stdio::null())
        .stderr(output.stdio().map_err(|e| e.to_string())?);
    let running =
        children::spawn(&mut agent).map_err(|e| format!("ssh-agent could not be started: {e}"))?;
    let deadline = Instant::now() + PATIENCE;
    loop {
        if std::fs::symlink_metadata(socket).is_ok_and(|meta| meta.file_type().is_socket()) {
            return Ok(());
        }
        if let Some(status) = children::wait_within(running.pid, Duration::from_millis(50)) {
            return Err(format!("ssh-agent {}", run::describe(status)));
        }
        if Instant::now() >= deadline {
            return Err("ssh-agent did not open its socket".to_string());
        }
    }
}

/// Hand the key to the agent through `ssh-add -`, which reads it from stdin.
fn add(key: &[u8], socket: &Path, user: &BuildUser) -> Result<(), String> {
    let mut ssh_add = user.command("ssh-add");
    ssh_add
        .arg("-")
        .env("SSH_AUTH_SOCK", socket)
        .stdin(Stdio::piped());
    let captured = run::capture(&mut ssh_add, Keep::Both)
        .and_then(|mut adding| {
            // A key is far smaller than a pipe holds, so this cannot block
            // on a reader that never comes. Dropping the handle ends
            // ssh-add's input.
            if let Some(mut stdin) = adding.stdin.take() {
                stdin.write_all(key)?;
            }
            adding.finish(Some(PATIENCE))
        })
        .map_err(|e| format!("ssh-add could not be run: {e}"))?;
    if captured.succeeded() {
        Ok(())
    } else {
        Err(format!(
            "ssh-add did not load the credential: {}",
            captured.text.trim()
        ))
    }
}
