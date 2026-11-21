//! OpenSSH's `ssh-agent`, holding the worker's SSH key for one build. See
//! "The SSH link" in `design/in-progress/container-workers.md`.

use anyhow::{Context, Result, bail};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use zeroize::Zeroizing;

const SOCKET: &str = "agent.sock";

/// How long ssh-agent has to make its socket (100 looks, 50 ms apart).
const SOCKET_POLLS: u32 = 100;
const SOCKET_POLL: Duration = Duration::from_millis(50);

/// How long `ssh-add` has to answer.
const ADD_TIME: Duration = Duration::from_secs(30);

/// The libraries ssh-agent may load for a smartcard or a security key: none.
/// The link passes no request that names one, and this says the same to the
/// agent itself.
const NO_PROVIDERS: &str = "";

/// A running ssh-agent that has the key, and nothing else.
///
/// Dropping it kills the agent and removes its socket.
pub struct Agent {
    socket: PathBuf,
    _child: Child,
    /// After the child, so the agent is gone before its directory is.
    _dir: tempfile::TempDir,
}

impl Agent {
    /// Start an agent and load the key in the file `key` into it.
    ///
    /// The key goes from the file to `ssh-add`'s standard input, so it is in
    /// no file but the one it came from, and in no process's arguments or
    /// environment.
    pub async fn start(key: &Path) -> Result<Self> {
        let material = Zeroizing::new(
            std::fs::read(key).with_context(|| format!("reading the SSH key {}", key.display()))?,
        );
        let dir = tempfile::Builder::new()
            .prefix("aurcache-ssh-agent-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .context("making a directory for ssh-agent's socket")?;
        let socket = dir.path().join(SOCKET);
        // `-D` keeps it in the foreground, as this process's child, so it
        // cannot outlive what started it unnoticed.
        let mut child = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .args(["-P", NO_PROVIDERS])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context(
                "starting ssh-agent, which holds the SSH key for a build: OpenSSH has to be \
                 installed where the worker runs",
            )?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(log(stderr));
        }
        wait_for_socket(&socket, &mut child).await?;
        add(&socket, &material)
            .await
            .with_context(|| format!("the SSH key {}", key.display()))?;
        Ok(Self {
            socket,
            _child: child,
            _dir: dir,
        })
    }

    /// The agent's socket, which only the worker's user can reach.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

/// What ssh-agent says, in the worker's log.
async fn log(stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!("ssh-agent: {line}");
    }
}

/// ssh-agent makes its socket a moment after it starts.
async fn wait_for_socket(socket: &Path, child: &mut Child) -> Result<()> {
    for _ in 0..SOCKET_POLLS {
        if socket.exists() {
            return Ok(());
        }
        if let Some(status) = child.try_wait().context("waiting for ssh-agent")? {
            bail!(
                "ssh-agent ended before it made its socket ({status}); the worker's debug log has what it said"
            );
        }
        tokio::time::sleep(SOCKET_POLL).await;
    }
    bail!("ssh-agent did not make {} in time", socket.display())
}

/// `ssh-add` against this agent, which never asks anyone for a passphrase:
/// there is no one to ask.
fn ssh_add(socket: &Path) -> Command {
    let mut command = Command::new("ssh-add");
    command
        .env("SSH_AUTH_SOCK", socket)
        .env("SSH_ASKPASS", "false")
        .env("SSH_ASKPASS_REQUIRE", "force")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Load the key into the agent through `ssh-add -`.
async fn add(socket: &Path, material: &[u8]) -> Result<()> {
    let mut child = ssh_add(socket)
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .context("running ssh-add")?;
    let mut stdin = child.stdin.take().context("ssh-add has no stdin")?;
    // ssh-add may stop reading at what is not a key, which is its to say.
    let _ = stdin.write_all(material).await;
    // It reads to the end of its input.
    drop(stdin);
    let out = tokio::time::timeout(ADD_TIME, child.wait_with_output())
        .await
        .context("ssh-add did not end in time")?
        .context("running ssh-add")?;
    if out.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&out.stderr);
    match said.trim() {
        "" => bail!(
            "ssh-add did not take it and gave no reason, which is what it does with a key \
             protected by a passphrase: the worker has no way to enter one"
        ),
        said => bail!("ssh-add did not take it: {said}"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command as Blocking;

    /// A key `ssh-keygen` made for one test.
    pub struct Generated {
        /// Holds the files for as long as the key is used.
        _dir: tempfile::TempDir,
        /// The private key's file.
        pub file: PathBuf,
        /// The line `ssh-keygen -l` prints for it.
        pub listed: String,
    }

    /// Make a key with `ssh-keygen`, unencrypted unless given a passphrase.
    pub fn keygen(kind: &str, passphrase: &str) -> Generated {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("key");
        let made = Blocking::new("ssh-keygen")
            .args(["-q", "-t", kind, "-N", passphrase, "-C", "a test key", "-f"])
            .arg(&file)
            .status()
            .expect("ssh-keygen is needed to make the test keys");
        assert!(made.success());
        let listed = Blocking::new("ssh-keygen")
            .arg("-lf")
            .arg(file.with_extension("pub"))
            .output()
            .unwrap();
        Generated {
            _dir: dir,
            file,
            listed: String::from_utf8(listed.stdout).unwrap().trim().to_string(),
        }
    }

    pub fn ed25519_key() -> Generated {
        keygen("ed25519", "")
    }

    /// The key that the agent at `socket` holds, as `ssh-add -l` lists it.
    pub fn listed(socket: &Path) -> String {
        let out = Blocking::new("ssh-add")
            .arg("-l")
            .env("SSH_AUTH_SOCK", socket)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[tokio::test]
    async fn the_agent_has_the_key_on_a_socket_of_its_own() {
        let key = ed25519_key();
        let agent = Agent::start(&key.file).await.unwrap();
        assert_eq!(listed(agent.socket()), key.listed);
        let socket = agent.socket().to_path_buf();
        let dir = socket.parent().unwrap().to_path_buf();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        assert!(tokio::net::UnixStream::connect(&socket).await.is_ok());

        drop(agent);
        assert!(!dir.exists());
        assert!(tokio::net::UnixStream::connect(&socket).await.is_err());
    }

    /// RSA and ECDSA keys are ssh-agent's to hold as well.
    #[tokio::test]
    async fn every_kind_of_key_ssh_add_takes_is_held() {
        for kind in ["rsa", "ecdsa"] {
            let key = keygen(kind, "");
            let agent = Agent::start(&key.file).await.unwrap();
            assert_eq!(listed(agent.socket()), key.listed, "{kind}");
        }
    }

    #[tokio::test]
    async fn a_key_with_a_passphrase_is_refused_saying_so() {
        let key = keygen("ed25519", "a passphrase");
        let said = format!("{:#}", Agent::start(&key.file).await.err().unwrap());
        assert!(said.contains("passphrase"), "{said}");
        assert!(said.contains(&key.file.display().to_string()), "{said}");
    }

    #[tokio::test]
    async fn what_is_not_a_key_is_refused_with_what_ssh_add_said() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("key");
        std::fs::write(&file, "not a key\n").unwrap();
        let said = format!("{:#}", Agent::start(&file).await.err().unwrap());
        assert!(said.contains("ssh-add did not take it: "), "{said}");
        assert!(!said.contains("passphrase"), "{said}");

        let missing = dir.path().join("missing");
        let said = format!("{:#}", Agent::start(&missing).await.err().unwrap());
        assert!(said.contains("reading the SSH key"), "{said}");
        assert!(said.contains("missing"), "{said}");
    }
}
