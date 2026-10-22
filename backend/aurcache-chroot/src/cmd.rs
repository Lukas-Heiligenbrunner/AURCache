//! Running the few commands that need root.
//!
//! The worker runs as an unprivileged user with `sudo` (see
//! `packaging/aurcache-worker.sudoers` for what that grant is and is not), and
//! every privileged action here goes through [`privileged`], so each one is a
//! single, logged argument vector. Run as root -- the hybrid image's
//! entrypoint, a test under `sudo` -- the command is run directly.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use tokio::process::Command;

/// Whether this process is already root, in which case `sudo` is skipped.
fn is_root() -> bool {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The command line for `args`, as root.
fn root_command<S: AsRef<OsStr>>(args: &[S]) -> std::process::Command {
    if is_root() {
        return user_command(args);
    }
    let mut cmd = std::process::Command::new("sudo");
    // `-n`: a missing grant is an error to report, never a password prompt
    // that hangs a worker with no terminal.
    cmd.arg("-n").args(args);
    cmd
}

/// The command line for `args`, as this process's own user.
fn user_command<S: AsRef<OsStr>>(args: &[S]) -> std::process::Command {
    let (program, rest) = args.split_first().expect("a command to run");
    let mut cmd = std::process::Command::new(program);
    cmd.args(rest);
    cmd
}

/// A finished command's stdout, or an error carrying its stderr.
fn stdout_of(shown: &str, output: std::io::Result<std::process::Output>) -> Result<String> {
    let output = output.with_context(|| format!("running {shown}"))?;
    if !output.status.success() {
        bail!(
            "{shown} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run `args` as root and return its stdout, or an error carrying its stderr.
pub async fn privileged<S: AsRef<OsStr>>(args: &[S]) -> Result<String> {
    let shown = display(args);
    tracing::debug!("$ {shown}");
    stdout_of(&shown, Command::from(root_command(args)).output().await)
}

/// Run an unprivileged query and return its stdout.
pub async fn query<S: AsRef<OsStr>>(args: &[S]) -> Result<String> {
    stdout_of(
        &display(args),
        Command::from(user_command(args)).output().await,
    )
}

fn display<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| a.as_ref().to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`privileged`], blocking: for code that already runs on a blocking thread.
pub fn privileged_blocking<S: AsRef<OsStr>>(args: &[S]) -> Result<String> {
    let shown = display(args);
    tracing::debug!("$ {shown}");
    stdout_of(&shown, root_command(args).output())
}

/// [`query`], blocking.
pub fn query_blocking<S: AsRef<OsStr>>(args: &[S]) -> Result<String> {
    stdout_of(&display(args), user_command(args).output())
}
