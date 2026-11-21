//! The build credential's ssh-agent.
//!
//! A build needs to *use* the worker's SSH key -- some PKGBUILDs fetch from
//! private repositories -- while never being able to read it. The key belongs
//! to the worker's user; builds run as a different, unprivileged one. An agent
//! is what bridges that: it holds the key and hands out signatures over a
//! socket, so a build authenticates without the key material ever being within
//! its reach.
//!
//! The agent runs *as the build user*. That is not a matter of taste --
//! OpenSSH's `ssh-agent` checks the peer uid on every connection and refuses
//! every one that is not its own, so an agent belonging to the worker cannot be
//! used by a build however the socket is owned or moded.
//!
//! Started here, by the worker, rather than by a shell script in the container
//! entrypoints. That was the arrangement before, and it meant the native
//! package had no agent at all -- the script lived under `docker/` and was
//! never packaged, so a natively installed worker could not fetch an
//! authenticated source however it was configured. It also had to run *before*
//! the worker, which is before the key it wanted to load necessarily existed,
//! so a fresh deployment silently started without one and needed a restart to
//! pick it up. Starting it here removes both: one mechanism, after the key is
//! ensured, on every platform.
//!
//! ## One agent per key
//!
//! Each key the worker holds -- `default`, and every one `WORKER_GIT_SSH_KEYS`
//! names -- is in an agent of its own, with its socket in a directory of its
//! own. A build is handed one socket and has that one directory bound into its
//! chroot, so it can use exactly the key it was offered and never reach
//! another through the same agent or the same mount.
//!
//! ## What a build can and cannot do with it
//!
//! It can ask the agent to authenticate, for anything, for as long as the build
//! runs -- no host restrictions are applied, because a source may legitimately
//! live on any host. That is accepted. What it cannot do is read the key, so
//! nothing it captures outlives the build.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

/// Directory the socket lives in, under the worker's data directory.
///
/// Deliberately not `/run`: that is root-owned on a host and the worker is not,
/// so putting it there needs `sudo` on the native path and a mkdir in the
/// container entrypoint -- the split this module exists to remove. The data
/// directory is already the worker's own on both.
const AGENT_DIR: &str = "agent";
const SOCKET_FILE: &str = "agent.sock";

/// The directory the agents' sockets live in, each in a directory of its own
/// below it, under the worker's data directory -- never in the storage pool,
/// so no kept build can hold one.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(AGENT_DIR)
}

/// The directory of the agent holding the key `name`: what a build offered
/// that key has bound into its chroot, and nothing beside it.
#[must_use]
pub fn key_dir(data_dir: &Path, name: &str) -> PathBuf {
    dir(data_dir).join(name)
}

/// The group shared by the worker and the users it builds as.
///
/// The socket's directory belongs to it, so the build user can create the
/// socket there and the worker can still watch for it appearing.
const BUILD_GROUP: &str = "aurbuild";

/// A running ssh-agent holding the build credential.
///
/// Killed when dropped, so the agent's lifetime is the worker's and no stray
/// agent survives a restart holding a key.
pub struct BuildAgent {
    name: String,
    socket: PathBuf,
    child: Child,
}

impl BuildAgent {
    /// The name of the key it holds.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The socket path, for `SSH_AUTH_SOCK` and for the chroot bind mount.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for BuildAgent {
    fn drop(&mut self) {
        // The process *group*: the agent runs under a `sudo` that may have
        // forked rather than exec'd in place, and which of the two happens is a
        // sudoers detail rather than something to depend on. `start` puts the
        // whole thing in its own group so this takes it down either way.
        if let Some(pid) = self.child.id() {
            // SAFETY: signalling a group we created ourselves; if it is already
            // gone the call fails harmlessly and we ignore the result.
            unsafe { libc::killpg(pid as libc::pid_t, libc::SIGTERM) };
        }
        self.child.start_kill().ok();
        std::fs::remove_file(&self.socket).ok();
    }
}

/// Start an agent holding `key`, and publish its socket to builds.
///
/// The agent is started **as `build_user`**, because `ssh-agent` serves only
/// its own uid. Started as the worker, it was reachable in the sense that a
/// build could open the socket -- and useless, because the agent hung up on it:
/// `communication with agent failed` on one side, `uid mismatch: peer euid 921
/// != uid 922` in the journal on the other.
///
/// Handing the agent to the build user does not hand it the key. The worker
/// reads the key with its own privileges and writes it into `ssh-add -`, which
/// takes a key on stdin, so the material goes from the worker's memory into the
/// agent without ever being a file the build user could open. What that user
/// gains is the ability to *use* the key while the agent lives, which is the
/// whole point, and an agent does not give back what it holds.
///
/// The agent holds the key `name` (a key name, which the caller has checked)
/// in [`key_dir`]. Returns `None` when there is no key to load, which is the
/// ordinary case for a worker that builds nothing needing authentication.
pub async fn start(
    data_dir: &Path,
    name: &str,
    key: &Path,
    build_user: &str,
) -> Result<Option<BuildAgent>> {
    if !key.exists() {
        return Ok(None);
    }

    let dir = key_dir(data_dir, name);
    ensure_dir(data_dir)?;
    make_group_dir(&dir)?;

    let socket = dir.join(SOCKET_FILE);
    // A socket left by a previous run: ssh-agent refuses to bind over it.
    std::fs::remove_file(&socket).ok();

    // `-D` keeps it in the foreground, so its lifetime is tied to this process
    // tree. The alternative -- letting it daemonize and parsing the shell
    // snippet it prints -- leaves an agent behind whenever the worker dies
    // badly.
    let socket_arg = socket.display().to_string();
    let mut spawn = as_user(build_user, &["ssh-agent", "-D", "-a", &socket_arg]);
    // Its own process group, so `Drop` can take down the agent together with
    // the `sudo` that started it.
    spawn.process_group(0);
    // Backstop for the window before `BuildAgent` (whose `Drop` kills the
    // group) exists: a socket timeout or a failed key load below must not
    // orphan the agent.
    spawn.kill_on_drop(true);
    let child = spawn.spawn().context("starting ssh-agent")?;

    wait_for_socket(&socket).await?;
    // The socket is left exactly as ssh-agent made it: the build user's own,
    // mode 0600. That is already the access we want -- it is the only uid the
    // agent will talk to -- and it is not ours to change.

    let material = std::fs::read(key).with_context(|| format!("reading {}", key.display()))?;
    add_key(build_user, &socket, &material)
        .await
        .context("loading the build credential into the agent")?;

    Ok(Some(BuildAgent {
        name: name.to_string(),
        socket,
        child,
    }))
}

/// Create the directory the agents' sockets live in, [`dir`], if it is not
/// there yet.
///
/// Before the sandbox is asked about it, too, which judges it where it really
/// is and so needs it to exist.
pub fn ensure_dir(data_dir: &Path) -> Result<()> {
    make_group_dir(&dir(data_dir))
}

/// Create `dir`, group-writable, because the process creating the socket is
/// the build user rather than us. It is in `aurbuild`; nobody else reaches
/// it, and no key is in it at all.
fn make_group_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    set_group_and_mode(dir, 0o770)
}

/// Write `material` into the running agent through `ssh-add -`.
async fn add_key(build_user: &str, socket: &Path, material: &[u8]) -> Result<()> {
    // `env` rather than `Command::env`: the command goes through `sudo`, which
    // strips the environment it was given.
    let auth_sock = format!("SSH_AUTH_SOCK={}", socket.display());
    let mut cmd = as_user(build_user, &["env", &auth_sock, "ssh-add", "-"]);
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running ssh-add")?;

    let mut stdin = child.stdin.take().context("ssh-add has no stdin")?;
    stdin
        .write_all(material)
        .await
        .context("writing the key to ssh-add")?;
    // Closed explicitly: ssh-add reads stdin to EOF, so holding the pipe open
    // would deadlock the wait below.
    drop(stdin);

    let out = child.wait_with_output().await.context("running ssh-add")?;
    if !out.status.success() {
        anyhow::bail!(
            "ssh-add refused the build credential: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Build a command that runs `argv` as `build_user`.
///
/// Directly when the worker already *is* that user, and through `sudo`
/// otherwise. The direct case is not a micro-optimisation: it is what lets a
/// container that builds as root work with no sudoers entry at all.
fn as_user(build_user: &str, argv: &[&str]) -> Command {
    let mut cmd = if is_current_user(build_user) {
        Command::new(argv[0])
    } else {
        // `-n`: fail rather than block on a password prompt nobody can answer.
        let mut sudo = Command::new("sudo");
        sudo.args(["-n", "-u", build_user]);
        sudo.arg(argv[0]);
        sudo
    };
    cmd.args(&argv[1..]);
    cmd
}

/// Whether `name` is the user this process is already running as.
///
/// Compared by uid rather than by name, so the two spellings of one account do
/// not send us through `sudo` to become ourselves.
#[cfg(unix)]
fn is_current_user(name: &str) -> bool {
    user_id(name) == Some(aurcache_chroot::Owner::current().uid)
}

/// A user's uid, through the passwd database (see
/// [`aurcache_chroot::Owner::of_user`]).
fn user_id(name: &str) -> Option<u32> {
    aurcache_chroot::Owner::of_user(name).map(|owner| owner.uid)
}

/// How long to wait for ssh-agent's socket (100 polls × 50 ms): it creates
/// the socket a moment after starting, and polling without a bound would hang
/// the job forever on an agent that never comes up.
const AGENT_SOCKET_POLLS: u32 = 100;
const AGENT_SOCKET_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// ssh-agent creates its socket a moment after starting.
async fn wait_for_socket(socket: &Path) -> Result<()> {
    for _ in 0..AGENT_SOCKET_POLLS {
        if socket.exists() {
            return Ok(());
        }
        tokio::time::sleep(AGENT_SOCKET_POLL_INTERVAL).await;
    }
    anyhow::bail!("ssh-agent did not create {} in time", socket.display())
}

/// Give the build group access, without granting it to everyone.
#[cfg(unix)]
fn set_group_and_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting {mode:o} on {}", path.display()))?;
    // Best-effort: a deployment without the build group still works, it just
    // runs builds as a user that can already reach the socket.
    if let Some(gid) = group_id(BUILD_GROUP) {
        chown_group(path, gid)
            .with_context(|| format!("giving {BUILD_GROUP} access to {}", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn group_id(name: &str) -> Option<u32> {
    // Through the group *database*, like `user_id` above: `/etc/group` is
    // only one NSS source, and hosts with LDAP/sssd/systemd-homed groups
    // would otherwise silently skip the chown and leave builds unable to
    // reach the agent socket.
    let c_name = std::ffi::CString::new(name).ok()?;
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::group = std::ptr::null_mut();
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: same shape as `user_id` above — NUL-terminated input that
    // outlives the call, valid out-parameters, and the `_r` form so
    // concurrent callers do not race on static storage.
    let rc = unsafe {
        libc::getgrnam_r(
            c_name.as_ptr(),
            &raw mut grp,
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut found,
        )
    };
    (rc == 0 && !found.is_null()).then_some(grp.gr_gid)
}

#[cfg(unix)]
fn chown_group(path: &Path, gid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `c_path` is a valid NUL-terminated path for the duration of the
    // call; -1 leaves the owner unchanged.
    let rc = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// The sandbox builds run under outside their chroot, as `makechrootpkg`
/// names it.
const SANDBOX: &str = "aurcache-sandbox";

// What the worker has to agree with `aurcache-sandbox` on to ask it. The
// sandbox is a program of its own, exec'd rather than linked, so these are
// copies of its definitions, which a test below holds to them.

/// The argument that makes the sandbox say what it would enforce, and run
/// nothing.
const CAPABILITIES_ARG: &str = "--capabilities";
/// The argument naming the agents' directory to that question.
const AGENT_DIR_ARG: &str = "--agent-dir";
/// The answer of a sandbox that would refuse a build every socket beneath
/// that directory but the one it is offered.
const RESOLVE_UNIX: &str = "resolve-unix";
/// The answer of one that would not, followed by `: ` and why not.
const NO_RESOLVE_UNIX: &str = "no-resolve-unix";

/// How long the sandbox has to answer. It reads one file and resolves a few
/// paths, which takes milliseconds; a question still unanswered after this --
/// a `sudo` waiting on something, a path on a network mount that is gone --
/// is given up on, as every build start waits for it.
const QUESTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether a build is kept from the agents of the keys it is not offered, on
/// this machine.
///
/// Inside its chroot it always is: only the directory of the agent it is
/// offered is mounted there. Outside it -- the PKGBUILD code `makechrootpkg`
/// runs on the worker -- it is the sandbox that refuses every other socket,
/// and only the sandbox can say whether it would: that takes a kernel that
/// can, and a policy the build user can read that protects the agents'
/// directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostSideGate {
    /// Refused outside the chroot as well.
    Enforced,
    /// The sandbox says a build would not be refused them; `why` is its
    /// reason.
    SandboxDeclines { why: String },
    /// The sandbox gave no answer to go by; `why` is what came of asking.
    Unanswered { why: String },
}

impl HostSideGate {
    /// Why builds are not kept from the agents outside their chroot, as a
    /// clause for the log; `None` when they are.
    #[must_use]
    pub fn gap(&self) -> Option<String> {
        match self {
            Self::Enforced => None,
            Self::SandboxDeclines { why } => Some(format!(
                "aurcache-sandbox, asked as the build user, says it would not: {why}"
            )),
            Self::Unanswered { why } => Some(format!(
                "aurcache-sandbox could not be asked whether it would, as the build user: {why}"
            )),
        }
    }
}

/// The agents of a worker's named keys, held only while the sandbox keeps
/// builds from them outside their chroot, asked again before each build: a
/// policy file edited or a sandbox package replaced under a running worker
/// changes the answer.
///
/// Withholding a named key from the build whose package names it would not
/// be enough once the answer changes: every other build's PKGBUILD code could
/// still reach the agent by its path. So on the first answer that is not
/// [`HostSideGate::Enforced`] -- a question that could not be asked counts --
/// the gate stops the agents, which removes their sockets, and holds none of
/// them again until the worker restarts.
pub struct NamedKeysGate {
    /// The question, as [`sandbox_capabilities`] puts it.
    ask: Box<dyn Fn() -> Command + Send + Sync>,
    /// The named keys' agents; empty once the gate has closed.
    agents: std::sync::Mutex<Vec<BuildAgent>>,
}

impl NamedKeysGate {
    /// For `agents`, started because the sandbox answered
    /// [`HostSideGate::Enforced`] for `build_user`'s builds and the agents
    /// under `data_dir`.
    #[must_use]
    pub fn new(build_user: &str, data_dir: &Path, agents: Vec<BuildAgent>) -> Self {
        let build_user = build_user.to_string();
        let data_dir = data_dir.to_path_buf();
        Self::asking(move || sandbox_capabilities(&build_user, &data_dir), agents)
    }

    fn asking(ask: impl Fn() -> Command + Send + Sync + 'static, agents: Vec<BuildAgent>) -> Self {
        Self {
            ask: Box::new(ask),
            agents: std::sync::Mutex::new(agents),
        }
    }

    /// The named keys' agents, locked.
    fn agents(&self) -> std::sync::MutexGuard<'_, Vec<BuildAgent>> {
        self.agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether the named keys are still held, asked of the sandbox now; on the
    /// first answer that builds are no longer kept from their agents, the
    /// agents are stopped for good, and the log says why, once.
    pub async fn holds(&self) -> bool {
        if self.agents().is_empty() {
            return false;
        }
        let Some(gap) = host_side_gate((self.ask)()).await.gap() else {
            // Closed meanwhile by another build's question, or still held.
            return !self.agents().is_empty();
        };
        let stopped = std::mem::take(&mut *self.agents());
        if !stopped.is_empty() {
            // Dropped, each agent is killed and its socket removed.
            drop(stopped);
            tracing::error!(
                "Builds can no longer be kept from the agents of the named SSH keys outside \
                 their chroot, because {gap}: this worker has stopped those agents, and a build \
                 whose package names one goes ahead without it until the worker restarts"
            );
        }
        false
    }
}

/// What the sandbox answers `query` ([`sandbox_capabilities`]), as a
/// [`HostSideGate`].
///
/// The sandbox answers with success whatever its answer, so a failure is its
/// own or that of `sudo` running it, given as the status and what it printed.
/// One that predates the question refuses it as a usage error, status 2.
/// One that does not answer within [`QUESTION_TIMEOUT`] gives no answer.
pub async fn host_side_gate(query: Command) -> HostSideGate {
    host_side_gate_within(query, QUESTION_TIMEOUT).await
}

/// [`host_side_gate`], with `within` for the sandbox to answer. Given up on,
/// the question is dropped, which kills it.
async fn host_side_gate_within(query: Command, within: Duration) -> HostSideGate {
    let out = match tokio::time::timeout(within, query_output(query)).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return HostSideGate::Unanswered {
                why: format!("{SANDBOX} {CAPABILITIES_ARG} could not run: {e}"),
            };
        }
        Err(_) => {
            return HostSideGate::Unanswered {
                why: format!("{SANDBOX} {CAPABILITIES_ARG} did not answer within {within:?}"),
            };
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return HostSideGate::Unanswered {
            why: format!("{}: {}", out.status, stderr.trim()),
        };
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // One line, and nothing beside it: that is all the sandbox prints, and
    // anything more is no answer this worker can read.
    let line = match stdout.lines().collect::<Vec<_>>().as_slice() {
        [line] => Some(line.trim()),
        _ => None,
    };
    if line == Some(RESOLVE_UNIX) {
        return HostSideGate::Enforced;
    }
    let declined = format!("{NO_RESOLVE_UNIX}: ");
    if let Some(why) = line.and_then(|line| line.strip_prefix(&declined)) {
        return HostSideGate::SandboxDeclines {
            why: why.to_string(),
        };
    }
    HostSideGate::Unanswered {
        why: format!(
            "{SANDBOX} {CAPABILITIES_ARG} answered neither {RESOLVE_UNIX} nor {NO_RESOLVE_UNIX} \
             on a line of its own (it printed {:?})",
            stdout.trim()
        ),
    }
}

/// Run the question, with nothing on its stdin.
async fn query_output(mut query: Command) -> std::io::Result<std::process::Output> {
    query
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
}

/// The command asking the `aurcache-sandbox` that confines `build_user`'s
/// builds whether it would keep them from the agents under `data_dir`.
///
/// Run as that user, the way `makechrootpkg` runs it, so the sandbox judges
/// the policy with that user's rights rather than the worker's. Through
/// `sudo -u` whoever the worker is, as `makechrootpkg` reaches the sandbox,
/// so sudo's own `PATH` finds the copy builds run: the worker's `PATH` need
/// not, even when the worker is the build user. A worker that `sudo` does
/// not let run as the build user gets no answer, and holds no named key.
#[must_use]
pub fn sandbox_capabilities(build_user: &str, data_dir: &Path) -> Command {
    let agents = dir(data_dir).display().to_string();
    let mut sudo = Command::new("sudo");
    // `-n`: fail rather than block on a password prompt nobody can answer.
    sudo.args([
        "-n",
        "-u",
        build_user,
        SANDBOX,
        CAPABILITIES_ARG,
        AGENT_DIR_ARG,
    ])
    .arg(agents);
    sudo
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worker's copies of what it asks the sandbox are the sandbox's own
    /// definitions, so the two cannot drift apart unnoticed.
    #[test]
    fn the_question_is_the_one_the_sandbox_answers() {
        let sandbox = include_str!("../../aurcache-sandbox/src/main.rs");
        for (name, value) in [
            ("CAPABILITIES_ARG", CAPABILITIES_ARG),
            ("AGENT_DIR_ARG", AGENT_DIR_ARG),
            ("RESOLVE_UNIX", RESOLVE_UNIX),
            ("NO_RESOLVE_UNIX", NO_RESOLVE_UNIX),
        ] {
            let definition = format!("const {name}: &str = {value:?};");
            assert!(
                sandbox.contains(&definition),
                "aurcache-sandbox does not define `{definition}`"
            );
        }
    }

    /// The gate `script` gives, run as the sandbox would be.
    async fn gate(script: &str) -> HostSideGate {
        let mut query = Command::new("sh");
        query.args(["-c", script]);
        host_side_gate(query).await
    }

    /// The reason of a gate that is not [`HostSideGate::Enforced`].
    fn why(gate: HostSideGate) -> String {
        match gate {
            HostSideGate::SandboxDeclines { why } | HostSideGate::Unanswered { why } => why,
            HostSideGate::Enforced => panic!("enforced"),
        }
    }

    /// Only a sandbox that answers the capability keeps builds from the
    /// agents; one that declines says why, which is the reason the worker
    /// logs.
    #[tokio::test]
    async fn only_a_sandbox_answering_resolve_unix_enforces() {
        assert_eq!(gate("echo resolve-unix").await, HostSideGate::Enforced);
        assert_eq!(
            gate("echo 'no-resolve-unix: bad policy (Permission denied)'").await,
            HostSideGate::SandboxDeclines {
                why: "bad policy (Permission denied)".into()
            }
        );
        for lacking in [
            "true",
            "echo resolve-unix-maybe",
            "echo other",
            "printf 'other\\nresolve-unix\\n'",
            "printf 'resolve-unix\\nresolve-unix\\n'",
            "printf 'no-resolve-unix: kernel (x)\\nresolve-unix\\n'",
        ] {
            let gate = gate(lacking).await;
            assert!(
                matches!(&gate, HostSideGate::Unanswered { why } if why.contains("answered neither")),
                "{lacking}: {gate:?}"
            );
        }
    }

    /// A question that fails gives no answer to go by, and says how it
    /// failed: a sandbox that predates it, and one that is not there.
    #[tokio::test]
    async fn a_failed_question_says_how_it_failed() {
        let old =
            why(gate("echo 'aurcache-sandbox: expected `--allow-build-env`' >&2; exit 2").await);
        assert!(old.contains("exit status: 2"), "{old}");
        assert!(old.contains("expected `--allow-build-env`"), "{old}");

        let missing = host_side_gate(Command::new("/nonexistent/aurcache-sandbox")).await;
        assert!(
            matches!(&missing, HostSideGate::Unanswered { why } if why.contains("could not run")),
            "{missing:?}"
        );
    }

    /// A question that does not answer in time gives no answer to go by,
    /// so a build start does not wait on it.
    #[tokio::test]
    async fn a_question_that_hangs_is_unanswered() {
        let mut query = Command::new("sh");
        query.args(["-c", "exec sleep 600"]);
        let started = std::time::Instant::now();
        let gate = host_side_gate_within(query, Duration::from_millis(200)).await;
        assert!(
            matches!(&gate, HostSideGate::Unanswered { why } if why.contains("did not answer")),
            "{gate:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(60));
    }

    /// A stand-in for a named key's agent: a process of its own group, and a
    /// socket in `dir`.
    fn stand_in_agent(dir: &Path, name: &str) -> BuildAgent {
        let socket = dir.join(format!("{name}.sock"));
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let mut spawn = Command::new("sleep");
        spawn.arg("600").process_group(0).kill_on_drop(true);
        BuildAgent {
            name: name.to_string(),
            socket,
            child: spawn.spawn().unwrap(),
        }
    }

    /// The gate holds while the sandbox answers `resolve-unix`. On the first
    /// answer that is not, it stops the agents -- their sockets go -- and holds
    /// none of them again, whatever the sandbox answers after.
    #[tokio::test]
    async fn the_named_keys_gate_stops_the_agents_once_it_closes() {
        let dir = tempfile::tempdir().unwrap();
        let answer = dir.path().join("answer");
        let script = answer.clone();
        let agent = stand_in_agent(dir.path(), "gitlab");
        let socket = agent.socket().to_path_buf();
        let gate = NamedKeysGate::asking(
            move || {
                let mut query = Command::new("cat");
                query.arg(&script);
                query
            },
            vec![agent],
        );
        std::fs::write(&answer, "resolve-unix\n").unwrap();
        assert!(gate.holds().await);
        assert!(socket.exists());

        std::fs::write(&answer, "no-resolve-unix: policy misses the agents (x)\n").unwrap();
        assert!(!gate.holds().await);
        assert!(!socket.exists());

        std::fs::write(&answer, "resolve-unix\n").unwrap();
        assert!(!gate.holds().await);
    }

    /// A question that cannot be asked closes the gate too.
    #[tokio::test]
    async fn a_failed_question_closes_the_named_keys_gate() {
        let dir = tempfile::tempdir().unwrap();
        let agent = stand_in_agent(dir.path(), "gitlab");
        let socket = agent.socket().to_path_buf();
        let gate = NamedKeysGate::asking(
            || Command::new("/nonexistent/aurcache-sandbox"),
            vec![agent],
        );
        assert!(!gate.holds().await);
        assert!(!socket.exists());
    }

    /// The question goes to the sandbox builds find, as the build user, and
    /// names the agents' directory under the worker's data directory.
    #[test]
    fn the_sandbox_is_asked_as_the_build_user() {
        // Through sudo even as the build user already, so it is sudo's `PATH`
        // that finds the sandbox, as it is for builds.
        let me = users_own_name().expect("`id -un` names this process's user");
        for user in ["someone-else", me.as_str()] {
            let cmd = sandbox_capabilities(user, Path::new("/srv/worker"));
            let argv: Vec<_> = cmd
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert_eq!(cmd.as_std().get_program(), "sudo");
            assert_eq!(
                argv,
                [
                    "-n",
                    "-u",
                    user,
                    SANDBOX,
                    CAPABILITIES_ARG,
                    AGENT_DIR_ARG,
                    "/srv/worker/agent"
                ]
            );
        }
    }

    /// The bind mount is the key's directory, not the socket: the socket's
    /// inode is replaced whenever the agent restarts, and a bind to the old
    /// one would survive as a broken mount. Each key has its own, below the
    /// one the sandbox's policy protects.
    #[test]
    fn each_key_has_a_directory_of_its_own() {
        let data = Path::new("/var/lib/aurcache-worker");
        let socket = key_dir(data, "gitlab").join(SOCKET_FILE);
        assert_eq!(
            socket.parent().unwrap(),
            Path::new("/var/lib/aurcache-worker/agent/gitlab")
        );
        assert_ne!(key_dir(data, "gitlab"), key_dir(data, "default"));
        for name in ["gitlab", "default"] {
            assert!(key_dir(data, name).starts_with(dir(data)));
        }
    }

    /// A worker with no credential is the ordinary case and must not fail.
    #[tokio::test]
    async fn no_key_means_no_agent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(
            start(dir.path(), "default", &missing, "builder")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The agent has to *be* the build user, because `ssh-agent` serves only
    /// its own uid. Running it as the worker is what made every authenticated
    /// fetch fail with `uid mismatch` while the socket looked perfectly
    /// reachable.
    #[test]
    fn a_different_build_user_is_reached_through_sudo() {
        let cmd = as_user("someone-else", &["ssh-agent", "-D", "-a", "/s"]);
        let std = cmd.as_std();
        let argv: Vec<_> = std
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(std.get_program(), "sudo");
        // `-n`, or a worker with no sudo rights hangs on a prompt instead of
        // reporting that it cannot start an agent.
        assert_eq!(argv[..3], ["-n", "-u", "someone-else"]);
        assert_eq!(argv[3], "ssh-agent");
        assert_eq!(argv.last().unwrap(), "/s");
    }

    /// Being the build user already -- a container that builds as root -- must
    /// not require a sudoers entry to start an agent.
    #[test]
    fn being_the_build_user_needs_no_sudo() {
        let me = users_own_name().expect("`id -un` names this process's user");
        let cmd = as_user(&me, &["ssh-agent", "-D"]);
        assert_ne!(cmd.as_std().get_program(), "sudo");
        assert_eq!(cmd.as_std().get_program(), "ssh-agent");
    }

    /// The uid lookup is what decides between those two, so it has to agree
    /// with the passwd database on a real account.
    #[test]
    fn the_uid_lookup_reads_the_passwd_database() {
        assert_eq!(user_id("root"), Some(0));
        assert!(user_id("definitely-not-a-real-user-name").is_none());
        assert!(is_current_user(&users_own_name().unwrap()));
        // The database, not the file: this host's own login has no line in
        // `/etc/passwd`, and resolving it is exactly what this must do.
        assert!(user_id(&users_own_name().unwrap()).is_some());
        assert!(!is_current_user("definitely-not-a-real-user-name"));
    }

    /// The name this process's uid goes by, asked of the system rather than of
    /// a file, so it answers on a host whose accounts are not local.
    fn users_own_name() -> Option<String> {
        let out = std::process::Command::new("id").arg("-un").output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// The group lookup reads the real database; an absent group is not an
    /// error, it just means nothing extra is granted.
    #[test]
    fn an_unknown_group_is_not_an_error() {
        assert!(group_id("definitely-not-a-real-group-name").is_none());
    }

    /// The gid lookup agrees with the group database on a real group, the
    /// way the uid lookup does for users.
    #[test]
    fn the_gid_lookup_reads_the_group_database() {
        assert_eq!(group_id("root"), Some(0));
        // This host's own primary group, asked of the system like the user
        // lookup above.
        let out = std::process::Command::new("id")
            .arg("-gn")
            .output()
            .expect("`id -gn` names this process's group");
        let mine = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(group_id(&mine), users_own_gid());
    }

    /// This process's own gid, for the lookup above to agree with.
    fn users_own_gid() -> Option<u32> {
        let out = std::process::Command::new("id").arg("-g").output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().parse().ok())?
    }
}
