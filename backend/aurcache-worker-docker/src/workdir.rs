//! One build's directory on this machine: made before the container, read
//! after it, and removed whatever happened.
//!
//! ```text
//! <dir>/            0755, this worker's; the agent takes it for root while it runs
//!   bin/aurcache-build-agent   0755, a copy of this worker's agent
//!   in/spec.json, in/source.tar.gz
//!   pkg/            what the build made, collected for upload
//!   out/report.json the agent's report, written last
//! ```

use anyhow::{Context, Result};
use aurcache_build_agent::layout::{BIN_NAME, OUT_DIR, REPORT_FILE, Work};
use aurcache_build_agent::report::BuildReport;
use aurcache_build_agent::spec::BuildSpec;
use aurcache_worker_container::input;
use aurcache_worker_core::beneath;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;

use crate::container::BIN_DIR;

/// The most a report may be: a few lines, and a commit per VCS source.
pub const REPORT_LIMIT: u64 = 1 << 20;

/// What the agent left at `out/report.json`.
#[derive(Debug, PartialEq)]
pub enum Report {
    Written(BuildReport),
    Absent,
    /// There is a file, but not a report this worker can read.
    Unreadable(String),
}

/// What removing a build directory as this worker's own user came to.
#[derive(Debug)]
pub enum Removal {
    /// It is gone, or was never there.
    Removed,
    /// Some of it is still there, usually what root in the build made its
    /// own; why, as far as `rm` or starting it says.
    Left(String),
}

/// Make `dir` for a build: the agent's copy of itself, the spec and the
/// source archive.
pub fn prepare(dir: &Path, agent: &Path, spec: &BuildSpec, source: &[u8]) -> Result<()> {
    make_dir(dir)?;
    let bin = dir.join(BIN_DIR);
    make_dir(&bin)?;
    let copy = bin.join(BIN_NAME);
    fs::copy(agent, &copy)
        .with_context(|| format!("copying the build agent from {}", agent.display()))?;
    set_mode(&copy, 0o755)?;
    input::write(dir, spec, source)
}

/// The report the agent left in `dir`.
///
/// Read as something the build could have written: root in the container
/// can replace `out/` or the report with a link, a pipe or a file without
/// end. [`beneath`] opens each name from its parent's handle without
/// following or waiting on it, and the report must be a regular file of at
/// most [`REPORT_LIMIT`] bytes. Blocking: call it off the async runtime.
#[must_use]
pub fn report(dir: &Path) -> Report {
    let read = beneath::Dir::open(dir)
        .and_then(|work| work.dir(OUT_DIR))
        .and_then(|out| out.read(REPORT_FILE, REPORT_LIMIT));
    match read {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(report) => Report::Written(report),
            Err(e) => Report::Unreadable(format!("{}: {e}", Work::new(dir).report().display())),
        },
        Err(e) if e.kind() == ErrorKind::NotFound => Report::Absent,
        Err(e) => Report::Unreadable(e.to_string()),
    }
}

/// Remove `dir` as this worker's own user, with `rm`: it follows no link,
/// walks a tree of any depth without running out of descriptors, and runs
/// off this process, so a large tree does not hold the async runtime up.
/// What it says goes nowhere: the build decides how much that is.
pub async fn remove(dir: &Path) -> Removal {
    let removed = tokio::process::Command::new("rm")
        .args(["-rf", "--"])
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await;
    match removed {
        Ok(status) if status.success() => Removal::Removed,
        Ok(status) => Removal::Left(format!("rm {}: {status}", dir.display())),
        Err(e) => Removal::Left(format!("running rm: {e}")),
    }
}

/// A directory with mode 0755 whatever the umask, so the agent's build user
/// can reach what is in it.
fn make_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    set_mode(dir, 0o755)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting the mode of {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_build_agent::report::Ended;
    use aurcache_build_agent::spec::INTERFACE;

    fn spec() -> BuildSpec {
        BuildSpec {
            interface: INTERFACE,
            pkgbase: "hello".into(),
            arch: "x86_64".into(),
            build_flags: vec![],
            makepkg_conf: String::new(),
            pacman_conf: "[options]\n".into(),
            pacman_upgrade_conf: "[options]\n".into(),
            mirrorlist: None,
            pgp_keys: vec![],
            keyserver: "hkps://keys.example".into(),
            cpus: None,
            scratch: "/var/lib/aurcache-build".into(),
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn prepare_lays_out_the_agent_and_the_inputs() {
        let scratch = tempfile::tempdir().unwrap();
        let agent = scratch.path().join("agent");
        fs::write(&agent, b"#!/bin/true\n").unwrap();
        set_mode(&agent, 0o700).unwrap();
        let dir = scratch.path().join("7");

        prepare(&dir, &agent, &spec(), b"source").unwrap();

        assert_eq!(mode(&dir), 0o755);
        assert_eq!(mode(&dir.join("bin")), 0o755);
        let copy = dir.join("bin").join(BIN_NAME);
        assert_eq!(fs::read(&copy).unwrap(), b"#!/bin/true\n");
        assert_eq!(mode(&copy), 0o755);
        let work = Work::new(&dir);
        assert_eq!(fs::read(work.source()).unwrap(), b"source");
        let written: BuildSpec = serde_json::from_slice(&fs::read(work.spec()).unwrap()).unwrap();
        assert_eq!(written, spec());
    }

    #[test]
    fn a_missing_agent_is_an_error_naming_it() {
        let scratch = tempfile::tempdir().unwrap();
        let error = prepare(
            &scratch.path().join("7"),
            &scratch.path().join("nowhere"),
            &spec(),
            b"",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("nowhere"), "{error:#}");
    }

    #[test]
    fn the_report_is_read_when_there_is_one() {
        let scratch = tempfile::tempdir().unwrap();
        assert_eq!(report(scratch.path()), Report::Absent);

        let written = BuildReport {
            interface: INTERFACE,
            ended: Ended::Exited { code: 0 },
        };
        let out = Work::new(scratch.path()).out();
        fs::create_dir_all(&out).unwrap();
        fs::write(
            out.join("report.json"),
            serde_json::to_vec(&written).unwrap(),
        )
        .unwrap();
        assert_eq!(report(scratch.path()), Report::Written(written));

        fs::write(out.join("report.json"), b"{\"interface\":").unwrap();
        assert!(matches!(report(scratch.path()), Report::Unreadable(_)));
    }

    /// Root in the container can make `out/` a link to a report that the
    /// build did not write; that report is refused. `beneath` tests the rest.
    #[test]
    fn a_report_through_a_link_is_refused() {
        let scratch = tempfile::tempdir().unwrap();
        let work = Work::new(scratch.path());
        let elsewhere = tempfile::tempdir().unwrap();
        let forged = Work::new(elsewhere.path());
        fs::create_dir_all(forged.out()).unwrap();
        fs::write(forged.report(), b"{}").unwrap();
        std::os::unix::fs::symlink(forged.out(), work.out()).unwrap();
        match report(scratch.path()) {
            Report::Unreadable(why) => assert!(why.contains("a link"), "{why}"),
            other => panic!("expected an unreadable report, found {other:?}"),
        }
    }

    /// Files, links and directories go; what a link points at stays. What is
    /// not there is removed already.
    #[tokio::test]
    async fn removing_a_tree_follows_no_link() {
        let scratch = tempfile::tempdir().unwrap();
        let elsewhere = scratch.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(elsewhere.join("kept"), b"x").unwrap();
        let dir = scratch.path().join("7");
        fs::create_dir_all(dir.join("pkg/a/b")).unwrap();
        fs::write(dir.join("pkg/a/b/file"), b"x").unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("pkg/link")).unwrap();
        std::os::unix::fs::symlink("/nonexistent", dir.join("dangling")).unwrap();

        assert!(matches!(remove(&dir).await, Removal::Removed));
        assert!(!dir.exists());
        assert!(elsewhere.join("kept").is_file());
        assert!(matches!(remove(&dir).await, Removal::Removed));
    }

    /// What this worker's user may not remove is left.
    #[tokio::test]
    async fn what_cannot_be_removed_is_left() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join("7");
        fs::create_dir_all(dir.join("locked/inner")).unwrap();
        set_mode(&dir.join("locked"), 0o500).unwrap();
        if fs::remove_dir(dir.join("locked/inner")).is_ok() {
            // Run as root, which may remove anything: nothing to test.
            return;
        }
        let removal = remove(&dir).await;
        set_mode(&dir.join("locked"), 0o700).unwrap();
        assert!(matches!(removal, Removal::Left(_)), "{removal:?}");
        assert!(dir.exists());
    }
}
