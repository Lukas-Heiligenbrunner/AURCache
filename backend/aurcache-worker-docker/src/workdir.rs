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
use aurcache_build_agent::layout::{BIN_NAME, Work};
use aurcache_build_agent::report::BuildReport;
use aurcache_build_agent::spec::BuildSpec;
use aurcache_worker_container::input;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::container::BIN_DIR;

/// What the agent left at `out/report.json`.
#[derive(Debug, PartialEq)]
pub enum Report {
    Written(BuildReport),
    Absent,
    /// There is a file, but not a report this worker can read.
    Unreadable(String),
}

/// What removing a build directory as this worker's own user came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Removal {
    /// It is gone, or was never there.
    Removed,
    /// Something in it is not this worker's to remove: a build that was
    /// stopped leaves the directory root's.
    NotPermitted,
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
#[must_use]
pub fn report(dir: &Path) -> Report {
    let path = Work::new(dir).report();
    match fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(report) => Report::Written(report),
            Err(e) => Report::Unreadable(format!("{}: {e}", path.display())),
        },
        Err(e) if e.kind() == ErrorKind::NotFound => Report::Absent,
        Err(e) => Report::Unreadable(format!("{}: {e}", path.display())),
    }
}

/// Remove `dir` as this worker's own user.
pub fn remove(dir: &Path) -> Result<Removal> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(Removal::Removed),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Removal::Removed),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => Ok(Removal::NotPermitted),
        Err(e) => Err(e).with_context(|| format!("removing {}", dir.display())),
    }
}

/// Whether `dir`, or the nearest directory above it that exists, is on a
/// filesystem this process sees mounted `noexec`. No, when that cannot be
/// read.
#[must_use]
pub fn noexec(dir: &Path) -> bool {
    let Some(existing) = dir.ancestors().find_map(|path| path.canonicalize().ok()) else {
        return false;
    };
    fs::read_to_string("/proc/self/mountinfo")
        .ok()
        .and_then(|mounts| mount_options(&mounts, &existing).map(has_noexec))
        .unwrap_or(false)
}

fn has_noexec(options: &str) -> bool {
    options.split(',').any(|option| option == "noexec")
}

/// The options of the mount `path` is on, from the text of
/// `/proc/self/mountinfo`: those of the longest mount point `path` is under,
/// and of the last such line, which is the mount on top.
fn mount_options<'a>(mountinfo: &'a str, path: &Path) -> Option<&'a str> {
    let mut found: Option<(usize, &str)> = None;
    for line in mountinfo.lines() {
        let mut fields = line.split(' ');
        let (Some(point), Some(options)) = (fields.nth(4), fields.next()) else {
            continue;
        };
        let point = unescaped(point);
        if !path.starts_with(&point) {
            continue;
        }
        if found.is_none_or(|(length, _)| point.len() >= length) {
            found = Some((point.len(), options));
        }
    }
    found.map(|(_, options)| options)
}

/// A mount point as the kernel writes it, with its `\040`-style escapes
/// (space, tab, newline, backslash) undone.
fn unescaped(point: &str) -> String {
    let mut out = String::with_capacity(point.len());
    let mut rest = point;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let code = rest.get(at + 1..at + 4);
        match code.and_then(|digits| u8::from_str_radix(digits, 8).ok()) {
            Some(byte) => {
                out.push(char::from(byte));
                rest = &rest[at + 4..];
            }
            None => {
                out.push('\\');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
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
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// A directory of this test's own, removed when it is dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "aurcache-worker-docker-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn spec() -> BuildSpec {
        BuildSpec {
            interface: INTERFACE,
            pkgbase: "hello".into(),
            build_flags: vec![],
            makepkg_conf: String::new(),
            pacman_conf: "[options]\n".into(),
            mirrorlist: None,
            pgp_keys: vec![],
            keyserver: "hkps://keys.example".into(),
            vcs_sources: vec![],
            cpus: None,
            scratch: "/var/lib/aurcache-build".into(),
            ssh: None,
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    const MOUNTS: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
30 22 0:25 / /tmp rw,nosuid,nodev shared:5 - tmpfs tmpfs rw
41 22 8:17 / /srv/builds rw,nosuid,nodev,noexec,relatime shared:9 - ext4 /dev/sdb1 rw
42 41 8:33 / /srv/builds/exec rw,relatime shared:10 - ext4 /dev/sdc1 rw
43 22 8:49 / /mnt/my\\040disk rw,noexec - ext4 /dev/sdd1 rw
44 22 0:40 / /over rw,noexec - tmpfs tmpfs rw
45 22 0:41 / /over rw,relatime - tmpfs tmpfs rw
";

    /// The mount a path is on is the deepest one above it, and the one on
    /// top where two are mounted at the same place.
    #[test]
    fn a_paths_mount_options_are_those_of_the_mount_it_is_on() {
        let on = |path: &str| mount_options(MOUNTS, Path::new(path)).unwrap();
        assert!(!has_noexec(on("/home/worker/builds")));
        assert!(!has_noexec(on("/tmp/aurcache-legacy-builds")));
        assert!(has_noexec(on("/srv/builds/7")));
        assert!(!has_noexec(on("/srv/builds/exec/7")));
        assert!(!has_noexec(on("/srv/buildsville")), "a name, not a prefix");
        assert!(has_noexec(on("/mnt/my disk/builds")));
        assert!(!has_noexec(on("/over/7")));
        assert_eq!(mount_options("", Path::new("/x")), None);
    }

    #[test]
    fn a_directory_that_is_not_there_yet_is_judged_by_what_is_above_it() {
        let scratch = Scratch::new("noexec");
        let above = noexec(&scratch.0);
        assert_eq!(noexec(&scratch.0.join("not/yet/there")), above);
    }

    #[test]
    fn prepare_lays_out_the_agent_and_the_inputs() {
        let scratch = Scratch::new("prepare");
        let agent = scratch.0.join("agent");
        fs::write(&agent, b"#!/bin/true\n").unwrap();
        set_mode(&agent, 0o700).unwrap();
        let dir = scratch.0.join("7");

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
        let scratch = Scratch::new("missing");
        let error = prepare(
            &scratch.0.join("7"),
            &scratch.0.join("nowhere"),
            &spec(),
            b"",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("nowhere"), "{error:#}");
    }

    #[test]
    fn the_report_is_read_when_there_is_one() {
        let scratch = Scratch::new("report");
        assert_eq!(report(&scratch.0), Report::Absent);

        let written = BuildReport {
            interface: INTERFACE,
            ended: Ended::Exited { code: 0 },
            peak_memory_bytes: Some(1),
            oom_kills: Some(0),
            disk_bytes: None,
            vcs_commits: BTreeMap::new(),
        };
        let out = Work::new(&scratch.0).out();
        fs::create_dir_all(&out).unwrap();
        fs::write(
            out.join("report.json"),
            serde_json::to_vec(&written).unwrap(),
        )
        .unwrap();
        assert_eq!(report(&scratch.0), Report::Written(written));

        fs::write(out.join("report.json"), b"{\"interface\":").unwrap();
        assert!(matches!(report(&scratch.0), Report::Unreadable(_)));
    }

    #[test]
    fn removing_what_is_not_there_is_done() {
        let scratch = Scratch::new("remove");
        let dir = scratch.0.join("7");
        fs::create_dir_all(dir.join("pkg")).unwrap();
        assert_eq!(remove(&dir).unwrap(), Removal::Removed);
        assert!(!dir.exists());
        assert_eq!(remove(&dir).unwrap(), Removal::Removed);
    }
}
