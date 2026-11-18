//! The dependencies a build installs before it starts, on this worker.
//!
//! makepkg would install them itself, through `sudo pacman`, and so run
//! their `.install` scriptlets and alpm hooks as root with the build's
//! network. The build script installs them instead, as root before the
//! build, downloading with network and installing with none
//! (`aurcache-sandbox offline`); the build then runs with `no_new_privs`
//! and cannot become root at all. makepkg still checks its dependencies,
//! and fails the build if anything it needs is missing.
//!
//! Which ones they are is [`aurcache_common::build_deps`], shared with the
//! build agent; this is how the chroot worker prints the `.SRCINFO` it reads
//! them from and hands them to the build script.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use aurcache_worker_core::protocol::Stop;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use aurcache_common::build_deps::SRCINFO_MAX;
pub use aurcache_common::build_deps::{Checks, Install, from_srcinfo};

/// The most of its error output that is read, for the message of a failure.
const ERRORS_MAX: u64 = 64 << 10;

/// The command that prints the `.SRCINFO` of the PKGBUILD in `pkgdir`:
/// `makepkg --printsrcinfo` run as `build_user`, in a process group of its
/// own, its output piped.
///
/// Printing it sources the PKGBUILD, so it is confined exactly as the
/// build script's own host-side sourcing is: `aurcache-sandbox
/// --allow-build-env`, writing nothing but `pkgdir`, reading none of the
/// worker's secrets. The caller places it in the build's cgroup.
#[must_use]
pub fn srcinfo_command(pkgdir: &Path, build_user: &str) -> Command {
    let mut cmd = Command::new("sudo");
    cmd.args([
        "-u",
        build_user,
        "aurcache-sandbox",
        "--allow-build-env",
        "--",
    ])
    .args(["makepkg", "--printsrcinfo"])
    .current_dir(pkgdir)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .process_group(0)
    .kill_on_drop(true);
    cmd
}

/// What came of printing a PKGBUILD's `.SRCINFO`.
#[derive(Debug, PartialEq, Eq)]
pub enum Printed {
    Srcinfo(String),
    /// The build had to stop first; the printing may still be running.
    Stopped(Stop),
}

/// The `.SRCINFO` `child` prints, unless `stopped` resolves first.
///
/// An error leaves the child possibly running, as a stop does: one that
/// printed too much is not waited for. The caller kills it either way.
pub async fn read_or_stop(
    child: &mut Child,
    stopped: impl Future<Output = Stop>,
) -> Result<Printed> {
    tokio::select! {
        srcinfo = read_srcinfo(child, SRCINFO_MAX, ERRORS_MAX) => srcinfo.map(Printed::Srcinfo),
        stop = stopped => Ok(Printed::Stopped(stop)),
    }
}

/// What `child` printed, once it has exited successfully: at most
/// `srcinfo_max` bytes of output, and `errors_max` of errors.
async fn read_srcinfo(child: &mut Child, srcinfo_max: u64, errors_max: u64) -> Result<String> {
    let stdout = child
        .stdout
        .take()
        .context("makepkg's output is not piped")?;
    let stderr = child
        .stderr
        .take()
        .context("makepkg's errors are not piped")?;
    let (srcinfo, errors, status) = tokio::try_join!(
        read_capped(stdout, srcinfo_max, "output"),
        read_capped(stderr, errors_max, "errors"),
        async {
            child
                .wait()
                .await
                .context("awaiting makepkg --printsrcinfo")
        },
    )?;
    if !status.success() {
        bail!(
            "makepkg --printsrcinfo failed ({status}): {}",
            String::from_utf8_lossy(&errors).trim()
        );
    }
    String::from_utf8(srcinfo).context("makepkg --printsrcinfo printed something not UTF-8")
}

/// All of `stream`, refused once it is longer than `max` bytes.
async fn read_capped(stream: impl AsyncRead + Unpin, max: u64, what: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream
        .take(max + 1)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("reading makepkg's {what}"))?;
    if bytes.len() as u64 > max {
        bail!("makepkg --printsrcinfo wrote more than {max} bytes of {what}");
    }
    Ok(bytes)
}

/// Write `deps` one per line to `dir`, for the build script
/// (`AURCACHE_BUILD_DEPS`).
///
/// In the worker's own staging directory, which the build user cannot
/// write: the script reads it as root and hands each line to pacman.
pub fn stage(dir: &Path, deps: &[String]) -> Result<PathBuf> {
    let path = dir.join("build-deps");
    let mut text = String::new();
    for dep in deps {
        text.push_str(dep);
        text.push('\n');
    }
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    fn shell(script: &str) -> Child {
        Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn a_srcinfo_is_read_in_full() {
        let mut child = shell("printf 'pkgbase = demo\\n'");
        assert_eq!(
            read_or_stop(&mut child, std::future::pending())
                .await
                .unwrap(),
            Printed::Srcinfo("pkgbase = demo\n".to_string())
        );
    }

    #[tokio::test]
    async fn a_failure_carries_its_errors() {
        let mut child = shell("echo 'ERROR: PKGBUILD does not exist.' >&2; exit 4");
        let err = read_or_stop(&mut child, std::future::pending())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("PKGBUILD does not exist"), "{err}");
    }

    /// A PKGBUILD that prints without end is refused at the cap, not held
    /// in memory, and not waited for either.
    #[tokio::test]
    async fn output_past_the_cap_is_refused() {
        let mut child = shell("yes pkgbase");
        let err = read_srcinfo(&mut child, 4096, 4096).await.unwrap_err();
        assert!(err.to_string().contains("more than 4096 bytes"), "{err}");
        let mut child = shell("yes error >&2");
        let err = read_srcinfo(&mut child, 4096, 4096).await.unwrap_err();
        assert!(err.to_string().contains("of errors"), "{err}");
    }

    /// A PKGBUILD that never finishes printing is left to the build's stop.
    #[tokio::test]
    async fn a_stop_ends_the_wait() {
        let mut child = shell("sleep 60");
        assert_eq!(
            read_or_stop(&mut child, async { Stop::Canceled })
                .await
                .unwrap(),
            Printed::Stopped(Stop::Canceled)
        );
    }

    #[test]
    fn staged_one_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = stage(dir.path(), &strings(&["glibc", "python>=3.11"])).unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "glibc\npython>=3.11\n"
        );
    }
}
