use std::path::{Path, PathBuf};
use std::process::Command;

use alpm_pkgbuild::bridge::BridgeOutput;
use alpm_srcinfo::SourceInfoV1;
use anyhow::Context;

/// Fix known-erroneous source URLs in PKGBUILD or .SRCINFO content that
/// makepkg accepts but `alpm-srcinfo` rejects.
///
/// Currently only fixes `?signed/` → `?signed` (trailing slash after the
/// signed flag in git VCS source URLs). Only affects libaegis and h2o-git.
pub fn fix_source_urls(content: &str) -> String {
    content.replace("?signed/", "?signed")
}

/// How a PKGBUILD is parsed: `alpm-pkgbuild-bridge`, confined by
/// `aurcache-sandbox`.
///
/// The bridge parses a PKGBUILD by *sourcing* it, so every parse runs
/// attacker-supplied bash in the process that owns the package database and
/// the repository. `SourceInfoV1::from_pkgbuild` would find the bridge through
/// `PATH` and hand it this process's environment, so confinement would hinge on
/// a wrapper shadowing it there -- and a missing wrapper would parse unconfined
/// without a word. This runs both by absolute path instead: without the
/// sandbox, parsing fails rather than proceeding.
///
/// What the parse may do is stated here; how that is enforced is entirely the
/// sandbox's business (its `parse` profile). The parse reads its PKGBUILD and
/// nothing under `private`, writes nothing, cannot reach the processes around
/// it, sees none of this process's environment, and touches the network only
/// when `network` is on.
#[derive(Debug, Clone)]
pub(crate) struct Bridge {
    sandbox: PathBuf,
    script: PathBuf,
    /// The server's own state, which a parse must not see.
    private: Vec<PathBuf>,
    /// Whether the parse may use the network (`parse_network`).
    network: bool,
}

/// How long a parse may take and how much it may print.
///
/// A parse runs the PKGBUILD's top level, which is attacker-supplied: it can
/// sleep forever, or print without end into the server's memory. Sourcing a
/// real recipe takes well under a second and prints a few kilobytes of
/// `.SRCINFO`; the minute is for the rare `pkgver=$(curl ...)` allowed the
/// network by `parse_network`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) timeout: std::time::Duration,
    /// Bytes of stdout, the bridge's output, before the parse is killed.
    pub(crate) max_stdout: usize,
    /// Bytes of stderr kept for the error message; the rest is discarded.
    pub(crate) max_stderr: usize,
}

pub(crate) const PARSE_LIMITS: Limits = Limits {
    timeout: std::time::Duration::from_secs(60),
    max_stdout: 8 << 20,
    max_stderr: 64 << 10,
};

/// What a bounded run produced.
#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// Run `command` to completion within `limits`, killing its whole process
/// group when it overruns either.
///
/// Its own process group, so the kill reaches whatever the PKGBUILD started --
/// `Child::kill` alone would stop the shell and leave a backgrounded `sleep`
/// holding the pipes. Output is read on threads so neither pipe can fill and
/// stall the child, and collecting it is bounded by the same deadline: a
/// process that escaped the group can keep a pipe open indefinitely.
pub(crate) fn run_bounded(mut command: Command, limits: Limits) -> anyhow::Result<BoundedOutput> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Instant;

    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("starting the parse")?;
    let group = i32::try_from(child.id()).context("process id")?;
    let kill_group = move || {
        // SAFETY: a plain kill(2) of a process group this function created.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    };

    /// What one pipe carried, up to its cap.
    #[derive(Default)]
    struct Captured {
        kept: Vec<u8>,
        /// Whether there was more than the cap.
        overflow: bool,
    }

    /// Read up to `cap` bytes, then keep draining so the writer never blocks.
    /// `on_overflow` runs once, the moment the cap is passed: a writer that
    /// never stops would otherwise only be noticed at the deadline.
    fn read_capped(mut from: impl Read, cap: usize, on_overflow: impl FnOnce()) -> Captured {
        let mut kept = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut on_overflow = Some(on_overflow);
        loop {
            match from.read(&mut chunk) {
                Ok(0) | Err(_) => {
                    return Captured {
                        kept,
                        overflow: on_overflow.is_none(),
                    };
                }
                Ok(n) => {
                    let room = cap.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                    if n > room
                        && let Some(stop) = on_overflow.take()
                    {
                        stop();
                    }
                }
            }
        }
    }

    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    let stdout = child.stdout.take().context("stdout was piped")?;
    let stderr = child.stderr.take().context("stderr was piped")?;
    let max_stdout = limits.max_stdout;
    std::thread::spawn(move || {
        // Past the cap, the parse is stopped at once.
        let _ = out_tx.send(read_capped(stdout, max_stdout, kill_group));
    });
    let max_stderr = limits.max_stderr;
    std::thread::spawn(move || {
        // Too much here only truncates the message.
        let _ = err_tx.send(read_capped(stderr, max_stderr, || {}));
    });

    let deadline = Instant::now() + limits.timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for the parse")? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_group();
            let _ = child.wait();
            anyhow::bail!(
                "the PKGBUILD took longer than {}s to parse; it was stopped",
                limits.timeout.as_secs()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    // Whatever it left running in its group goes too: nothing a parse starts
    // outlives it.
    kill_group();

    let remaining = || deadline.saturating_duration_since(Instant::now());
    let stdout = out_rx
        .recv_timeout(remaining())
        .context("the parse's output was never closed")?;
    let stderr = err_rx.recv_timeout(remaining()).unwrap_or_default();
    if stdout.overflow {
        anyhow::bail!(
            "the PKGBUILD printed more than {} MiB while being parsed; it was stopped",
            limits.max_stdout >> 20
        );
    }
    Ok(BoundedOutput {
        status,
        stdout: stdout.kept,
        stderr: stderr.kept,
    })
}

/// Paths outside the server's working directory that hold its secrets.
///
/// The native packages put the server's environment file here, with the
/// database password and the OAuth secret in it. A path that does not exist
/// costs nothing to name.
const PRIVATE_PATHS: [&str; 1] = ["/etc/aurcache"];

/// The server's own state, each directory as this process resolves it.
///
/// Named one by one rather than as the working directory: the repository
/// beside them is public, and the parse is not kept from what anyone can
/// download.
fn private_state() -> Vec<PathBuf> {
    vec![
        // SQLite's directory; see `aurcache_db::init_db`.
        PathBuf::from("./db"),
        aurcache_common::fs::ca_dir(),
        aurcache_common::fs::build_log_root(),
        // Every package's checkout, private sources included.
        crate::snapshot::default_checkout_root(),
    ]
}

impl Bridge {
    /// Resolve the installed layout.
    ///
    /// The defaults are the native packages' paths; the container images
    /// install under `/usr/local` and set `AURCACHE_SANDBOX` and
    /// `AURCACHE_PKGBUILD_BRIDGE` accordingly.
    pub(crate) fn from_env(network: bool) -> anyhow::Result<Self> {
        let path_from = |key: &str, default: &str| {
            std::env::var_os(key)
                .filter(|v| !v.is_empty())
                .map_or_else(|| PathBuf::from(default), PathBuf::from)
        };
        let cwd = std::env::current_dir()
            .context("cannot determine the directory to protect from PKGBUILD parsing")?;
        // `AURCACHE_PROTECTED_DIR` adds to the defaults rather than replacing
        // them, so a deployment naming one more directory cannot accidentally
        // expose the server's own.
        let extra = std::env::var_os("AURCACHE_PROTECTED_DIR").filter(|v| !v.is_empty());
        // Absolute, because the sandbox is not started where the server runs.
        let mut private: Vec<PathBuf> = private_state()
            .into_iter()
            .chain(PRIVATE_PATHS.map(PathBuf::from))
            .chain(extra.iter().flat_map(std::env::split_paths))
            .map(|dir| cwd.join(dir))
            .collect();
        private.dedup();

        Ok(Self {
            network,
            sandbox: path_from("AURCACHE_SANDBOX", "/usr/bin/aurcache-sandbox"),
            script: path_from("AURCACHE_PKGBUILD_BRIDGE", "/usr/bin/alpm-pkgbuild-bridge"),
            private,
        })
    }

    /// Whether both executables exist, for tests that need a real parse.
    #[cfg(test)]
    pub(crate) fn is_installed(&self) -> bool {
        self.sandbox.is_file() && self.script.is_file()
    }

    /// The confined invocation that parses the PKGBUILD at `pkgbuild`.
    fn command(&self, pkgbuild: &Path) -> anyhow::Result<Command> {
        let pkgbuild = std::path::absolute(pkgbuild)
            .with_context(|| format!("cannot resolve {}", pkgbuild.display()))?;

        let mut command = Command::new(&self.sandbox);
        command.arg("parse").arg("--pkgbuild").arg(&pkgbuild);
        for private in &self.private {
            command.arg("--private").arg(private);
        }
        if self.network {
            command.arg("--network");
        }
        command.arg("--").arg(&self.script);
        Ok(command)
    }

    /// Parse the PKGBUILD at `pkgbuild`.
    pub(crate) fn parse(&self, pkgbuild: &Path) -> anyhow::Result<SourceInfoV1> {
        let output = run_bounded(self.command(pkgbuild)?, PARSE_LIMITS).with_context(|| {
            format!(
                "cannot parse {} with {}",
                pkgbuild.display(),
                self.sandbox.display()
            )
        })?;
        if !output.status.success() {
            // The bridge reports some errors on stdout, the sandbox on stderr.
            anyhow::bail!(
                "parsing {} failed ({}): {} {}",
                pkgbuild.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
                String::from_utf8_lossy(&output.stdout).trim(),
            );
        }
        let stdout = std::str::from_utf8(&output.stdout)
            .context("alpm-pkgbuild-bridge printed invalid UTF-8")?;
        let bridge = BridgeOutput::from_script_output(stdout)?;
        Ok(bridge.try_into()?)
    }
}

/// Commands that need the network, in the shapes a PKGBUILD uses to compute a
/// version while it is being sourced.
const NETWORK_COMMANDS: [&str; 6] = [
    "curl",
    "wget",
    "aria2c",
    "git ls-remote",
    "git fetch",
    "svn",
];

/// Say so when a parse failed because the PKGBUILD wanted the network.
///
/// A denied `curl` leaves `pkgver` empty, and the parse then fails somewhere
/// else entirely -- on a missing version, not on the connection. Without this
/// the operator sees a version error and has no way to reach the setting that
/// fixes it. Only the code that actually runs is considered: everything from
/// the first function definition on is not executed by sourcing the file.
fn explain_network_need(error: anyhow::Error, content: &str, network: bool) -> anyhow::Error {
    if network {
        return error;
    }
    // Borrowed lines, not a joined `String` copy: the question is which
    // network command the top-level lines name, and the order asked is
    // commands-first (the first listed command found anywhere wins).
    let top_level: Vec<&str> = content
        .lines()
        .take_while(|line| !line.contains("() {") && !line.trim_end().ends_with("()"))
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect();
    let Some(command) = NETWORK_COMMANDS
        .into_iter()
        .find(|command| top_level.iter().any(|line| line.contains(command)))
    else {
        return error;
    };
    error.context(format!(
        "this PKGBUILD runs `{command}` while it is parsed, and parsing is \
         confined without network access; set `parse_network` if this source \
         is trusted"
    ))
}

/// Parse a PKGBUILD file, applying workarounds for common issues found in
/// real-world PKGBUILDs that makepkg accepts but `alpm-srcinfo` rejects.
pub fn parse_pkgbuild(path: &Path, network: bool) -> anyhow::Result<SourceInfoV1> {
    let bridge = Bridge::from_env(network)?;
    bridge
        .parse(path)
        .or_else(|error| retry_fixed(&bridge, error, &std::fs::read_to_string(path)?))
}

/// Parse PKGBUILD content held in memory, applying the same workarounds as
/// [`parse_pkgbuild`]. Since the bridge is a bash script that needs a real
/// filesystem path to `source` the PKGBUILD, the content is written to a
/// short-lived temp file that's removed again as soon as parsing finishes - no
/// other part of this function touches disk.
pub fn parse_pkgbuild_content(content: &str, network: bool) -> anyhow::Result<SourceInfoV1> {
    let bridge = Bridge::from_env(network)?;
    parse_in_temp(&bridge, content).or_else(|error| retry_fixed(&bridge, error, content))
}

/// Parse `content` from a temporary `PKGBUILD`.
fn parse_in_temp(bridge: &Bridge, content: &str) -> anyhow::Result<SourceInfoV1> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("PKGBUILD");
    std::fs::write(&path, content)?;
    let result = bridge.parse(&path);
    dir.close()?;
    result
}

/// Parse `content` again with its known-bad source URLs fixed, after the
/// parse of it as written failed with `error`.
///
/// When there is nothing to fix, `error` stands -- explained, if the
/// PKGBUILD wanted the network.
fn retry_fixed(
    bridge: &Bridge,
    error: anyhow::Error,
    content: &str,
) -> anyhow::Result<SourceInfoV1> {
    let fixed = fix_source_urls(content);
    if fixed == content {
        return Err(explain_network_need(error, content, bridge.network));
    }
    parse_in_temp(bridge, &fixed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The bridge and the sandbox are installed by the images, not by a plain
    /// `cargo test`, so tests that parse a PKGBUILD skip without them. CI sets
    /// `AURCACHE_SANDBOX` and `AURCACHE_PKGBUILD_BRIDGE`; there a missing
    /// executable is a misconfigured job, not a reason to skip.
    pub(crate) fn bridge_available() -> bool {
        let available = Bridge::from_env(false).is_ok_and(|bridge| bridge.is_installed());
        assert!(
            available || std::env::var_os("CI").is_none(),
            "AURCACHE_SANDBOX and AURCACHE_PKGBUILD_BRIDGE must name installed executables in CI"
        );
        available
    }

    fn bridge_protecting(private: &Path) -> Bridge {
        Bridge {
            private: vec![private.to_path_buf()],
            ..Bridge::from_env(false).unwrap()
        }
    }

    fn fake_bridge() -> Bridge {
        Bridge {
            sandbox: PathBuf::from("/opt/sandbox"),
            script: PathBuf::from("/opt/bridge"),
            private: vec![PathBuf::from("/srv/db"), PathBuf::from("/etc/aurcache")],
            network: false,
        }
    }

    /// The invocation states what the parse may do and nothing about how: the
    /// PKGBUILD, every private path, and the script by absolute path rather
    /// than whatever `PATH` finds.
    #[test]
    fn command_states_the_parse_profile() {
        let command = fake_bridge()
            .command(Path::new("/work/demo/PKGBUILD"))
            .unwrap();

        assert_eq!(command.get_program(), "/opt/sandbox");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "parse",
                "--pkgbuild",
                "/work/demo/PKGBUILD",
                "--private",
                "/srv/db",
                "--private",
                "/etc/aurcache",
                "--",
                "/opt/bridge",
            ]
        );
    }

    /// The sandbox refuses a relative path rather than guess what it was
    /// relative to.
    #[test]
    fn command_makes_the_pkgbuild_absolute() {
        let command = fake_bridge().command(Path::new("demo/PKGBUILD")).unwrap();
        let pkgbuild = PathBuf::from(command.get_args().nth(2).unwrap());
        assert!(pkgbuild.is_absolute());
        assert!(pkgbuild.ends_with("demo/PKGBUILD"));
    }

    /// The server's state is private, and resolved against where the server
    /// runs: the sandbox is not started there. (Only paths no other test
    /// moves through the environment are checked by name.)
    #[test]
    fn the_servers_state_is_private_and_absolute() {
        let bridge = Bridge::from_env(false).unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert!(
            bridge.private.contains(&cwd.join("db")),
            "{:?}",
            bridge.private
        );
        assert!(bridge.private.contains(&PathBuf::from("/etc/aurcache")));
        assert_eq!(bridge.private.len(), 5, "{:?}", bridge.private);
        assert!(
            bridge.private.iter().all(|p| p.is_absolute()),
            "{:?}",
            bridge.private
        );
    }

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    fn limits(timeout_ms: u64) -> Limits {
        Limits {
            timeout: std::time::Duration::from_millis(timeout_ms),
            max_stdout: 1 << 20,
            max_stderr: 1024,
        }
    }

    fn alive(pid: &str) -> bool {
        Path::new("/proc").join(pid.trim()).exists()
    }

    #[test]
    fn a_bounded_run_returns_status_and_both_streams() {
        let out = run_bounded(sh("echo hi; echo err >&2; exit 3"), limits(10_000)).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"hi\n");
        assert_eq!(out.stderr, b"err\n");
    }

    /// A PKGBUILD that never finishes is stopped at the deadline, and what it
    /// started in the background goes with it.
    #[test]
    fn a_parse_that_hangs_is_stopped_with_everything_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let started = std::time::Instant::now();
        let error = run_bounded(
            sh(&format!(
                "sleep 300 & echo $! > {}; wait",
                pidfile.display()
            )),
            limits(500),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("longer than"), "{error:#}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!alive(&pid), "the backgrounded sleep outlived the parse");
    }

    /// Printing without end is stopped at the cap, not at the deadline, and
    /// never all held in memory.
    #[test]
    fn a_parse_that_floods_its_output_is_stopped_at_the_cap() {
        let started = std::time::Instant::now();
        let error = run_bounded(sh("yes"), limits(30_000)).unwrap_err();
        assert!(
            format!("{error:#}").contains("printed more than"),
            "{error:#}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "stopped at the cap, well before the deadline"
        );
    }

    /// A background process holding the pipe open must not stall a parse that
    /// has finished: it is killed with the rest of the group.
    #[test]
    fn a_finished_parse_is_not_held_open_by_what_it_backgrounded() {
        let started = std::time::Instant::now();
        let out = run_bounded(sh("sleep 300 & echo done"), limits(10_000)).unwrap();
        assert_eq!(out.stdout, b"done\n");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// Too much stderr is truncated for the message, not an error.
    #[test]
    fn excess_stderr_is_truncated() {
        let out = run_bounded(sh("head -c 5000 /dev/zero >&2; echo ok"), limits(10_000)).unwrap();
        assert_eq!(out.stderr.len(), 1024);
        assert_eq!(out.stdout, b"ok\n");
    }

    /// Missing the sandbox is an error, never an unconfined parse.
    #[test]
    fn a_missing_sandbox_fails_the_parse() {
        let dir = tempfile::tempdir().unwrap();
        let pkgbuild = dir.path().join("PKGBUILD");
        std::fs::write(&pkgbuild, "pkgname=demo\npkgver=1\npkgrel=1\narch=(any)\n").unwrap();
        let bridge = Bridge {
            sandbox: dir.path().join("no-such-sandbox"),
            ..fake_bridge()
        };
        let error = bridge.parse(&pkgbuild).unwrap_err();
        assert!(
            format!("{error:#}").contains("no-such-sandbox"),
            "{error:#}"
        );
    }

    /// The same parse with no sandbox at all, as `SourceInfoV1::from_pkgbuild`
    /// would do it.
    ///
    /// Every assertion about a denial is paired with one of these, because an
    /// allow-list fails open: that a fixture did not read a file means nothing
    /// unless the same fixture demonstrably reads it unconfined. A policy
    /// widened by accident turns this leg's assertion red.
    fn parse_unconfined(pkgbuild: &Path) -> String {
        let output = Command::new(Bridge::from_env(false).unwrap().script)
            .current_dir(pkgbuild.parent().unwrap())
            .arg(pkgbuild.file_name().unwrap())
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /// End to end against the real sandbox: a PKGBUILD can neither read the
    /// protected directory nor write anywhere, its own directory included --
    /// and it still parses from inside a protected directory, which is where
    /// the server keeps its checkouts.
    #[test]
    fn a_parse_cannot_read_protected_files_or_write_anywhere() {
        if !bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }
        let protected = tempfile::tempdir().unwrap();
        // Closed to other users, as the server's own state directories are
        // required to be: a host where they are not gets a refusal instead.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(protected.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        std::fs::write(protected.path().join("secret"), "2").unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let marker = elsewhere.path().join("marker");
        let work = protected.path().join("checkout");
        std::fs::create_dir(&work).unwrap();
        let own_marker = work.join("marker");
        let pkgbuild = work.join("PKGBUILD");
        std::fs::write(
            &pkgbuild,
            format!(
                "pkgname=demo\n\
                 pkgver=$(cat {secret} 2>/dev/null || echo 1)\n\
                 pkgrel=1\n\
                 arch=(any)\n\
                 $(echo touched > {marker} 2>/dev/null)\n\
                 $(echo touched > {own_marker} 2>/dev/null)\n\
                 package() {{ :; }}\n",
                secret = protected.path().join("secret").display(),
                marker = marker.display(),
                own_marker = own_marker.display(),
            ),
        )
        .unwrap();

        let unconfined = parse_unconfined(&pkgbuild);
        assert!(
            unconfined.contains("pkgver \"2\""),
            "fixture proves nothing: it did not read the secret unconfined: {unconfined}"
        );
        assert!(marker.exists(), "fixture did not write the marker either");
        assert!(own_marker.exists(), "fixture did not write beside itself");
        std::fs::remove_file(&marker).unwrap();
        std::fs::remove_file(&own_marker).unwrap();

        let info = bridge_protecting(protected.path())
            .parse(&pkgbuild)
            .unwrap();

        assert_eq!(
            info.base.version.to_string(),
            "1-1",
            "read the protected file"
        );
        assert!(!marker.exists(), "wrote outside the PKGBUILD's directory");
        assert!(!own_marker.exists(), "wrote in the PKGBUILD's directory");
    }

    /// With `parse_network` on, the parse is told so -- and nothing else
    /// about its profile changes.
    #[test]
    fn allowing_the_network_is_one_flag() {
        let bridge = Bridge {
            network: true,
            ..fake_bridge()
        };
        let with: Vec<_> = bridge
            .command(Path::new("/work/demo/PKGBUILD"))
            .unwrap()
            .get_args()
            .map(std::ffi::OsStr::to_os_string)
            .collect();
        let without: Vec<_> = fake_bridge()
            .command(Path::new("/work/demo/PKGBUILD"))
            .unwrap()
            .get_args()
            .map(std::ffi::OsStr::to_os_string)
            .collect();

        let added: Vec<_> = with.iter().filter(|a| !without.contains(a)).collect();
        assert_eq!(added, ["--network"]);
        assert_eq!(with.len(), without.len() + 1);
    }

    /// A PKGBUILD that fetches its version while being sourced fails on the
    /// missing version, not on the denied connection, so the error has to name
    /// the cause and the setting that lifts it.
    #[test]
    fn a_failed_parse_names_the_setting_when_the_pkgbuild_wants_the_network() {
        let pkgbuild = "pkgname=demo\n\
                        pkgver=$(curl -s https://api.example.com/latest)\n\
                        package() { :; }\n";
        let explained = explain_network_need(anyhow::anyhow!("missing pkgver"), pkgbuild, false);
        let message = format!("{explained:#}");
        assert!(message.contains("curl"), "{message}");
        assert!(message.contains("parse_network"), "{message}");

        // With the setting already on, the failure is something else and must
        // not be dressed up as this one.
        let unchanged = explain_network_need(anyhow::anyhow!("missing pkgver"), pkgbuild, true);
        assert_eq!(format!("{unchanged:#}"), "missing pkgver");
    }

    /// The hint is only for code that runs: `curl` inside `package()` is not
    /// executed by sourcing the file, so it explains nothing.
    #[test]
    fn the_hint_ignores_network_use_inside_functions() {
        let pkgbuild = "pkgname=demo\n\
                        pkgver=1\n\
                        package() {\n  curl -s https://api.example.com/x\n}\n";
        let error = explain_network_need(anyhow::anyhow!("boom"), pkgbuild, false);
        assert_eq!(format!("{error:#}"), "boom");
    }

    /// A parse has no business connecting anywhere. The attempt fails before
    /// reaching the port -- distinct from the "Connection refused" an
    /// unconfined attempt gets from a closed one. This is hardening the kernel
    /// provides where it can, so CI does not hold a Landlock-less run to it.
    #[test]
    fn a_parse_cannot_open_a_tcp_connection() {
        if !bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let pkgbuild = work.path().join("PKGBUILD");
        std::fs::write(
            &pkgbuild,
            "pkgname=demo\n\
             pkgver=$( (exec 3<>/dev/tcp/127.0.0.1/1) 2>&1 \
                 | grep -q 'refused' && echo 2 || echo 1)\n\
             pkgrel=1\n\
             arch=(any)\n\
             package() { :; }\n",
        )
        .unwrap();

        let unconfined = parse_unconfined(&pkgbuild);
        assert!(
            unconfined.contains("pkgver \"2\""),
            "fixture proves nothing: the connection was refused rather than denied: {unconfined}"
        );

        let info = Bridge::from_env(false).unwrap().parse(&pkgbuild).unwrap();

        assert_eq!(info.base.version.to_string(), "1-1", "TCP was reachable");
    }

    #[test]
    fn renders_srcinfo_from_pkgbuild_content() {
        if !bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }
        let srcinfo = parse_pkgbuild_content(
            "pkgname=demo\npkgver=1.2\npkgrel=3\narch=(any)\npackage() { :; }\n",
            false,
        )
        .unwrap()
        .as_srcinfo();
        assert!(srcinfo.contains("pkgbase = demo"), "{srcinfo}");
        assert!(srcinfo.contains("pkgver = 1.2"), "{srcinfo}");
    }
}
