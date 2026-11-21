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
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

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
    /// Some of it is still there. A build that was stopped leaves the
    /// directory root's, and root in a build container can leave a tree
    /// deeper than this process may hold directories open to walk.
    Left(std::io::Error),
}

/// Who besides its owner can reach what is in a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Its owner alone: mode 0700, or narrower.
    Owner,
    /// Others as well, through this mode.
    Others { mode: u32 },
}

/// Make the directory builds go in, mode 0700, unless it is there already.
///
/// Root in a build container can leave a setuid-root file in its build's
/// directory, and Docker cannot mount a bind `nosuid`. Inside a directory
/// only its owner can enter, nobody else on the host can run one.
pub fn make_root(dir: &Path) -> Result<()> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != ErrorKind::AlreadyExists => {
            Err(e).with_context(|| format!("creating {}", dir.display()))
        }
        _ => Ok(()),
    }
}

/// Who besides its owner can reach what is in `dir`.
pub fn reach(dir: &Path) -> Result<Reach> {
    let mode = fs::metadata(dir)
        .with_context(|| format!("reading the mode of {}", dir.display()))?
        .permissions()
        .mode()
        & 0o7777;
    Ok(if mode & 0o077 == 0 {
        Reach::Owner
    } else {
        Reach::Others { mode }
    })
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

/// Remove `dir` as this worker's own user.
pub fn remove(dir: &Path) -> Removal {
    match fs::remove_dir_all(dir) {
        Ok(()) => Removal::Removed,
        Err(e) if e.kind() == ErrorKind::NotFound => Removal::Removed,
        Err(e) => Removal::Left(e),
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
            pacman_upgrade_conf: "[options]\n".into(),
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

    /// `report` on a thread, failing rather than hanging when it does not
    /// return.
    fn report_promptly(dir: &Path) -> Report {
        let (send, receive) = std::sync::mpsc::channel();
        let dir = dir.to_path_buf();
        std::thread::spawn(move || send.send(report(&dir)));
        receive
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the report is read, or refused, at once")
    }

    fn unreadable(found: Report) -> String {
        match found {
            Report::Unreadable(why) => why,
            other => panic!("expected an unreadable report, found {other:?}"),
        }
    }

    /// What root in the container can leave where the report goes: a pipe
    /// nothing will write to, which a plain read waits on for ever; a link to
    /// a device without end, or to a report the build did not write; an
    /// `out/` that is a link; a file past the limit. Each is refused, at
    /// once.
    #[test]
    fn a_report_that_is_not_a_file_of_its_own_is_refused_at_once() {
        let scratch = Scratch::new("hostile-report");
        let work = Work::new(&scratch.0);
        fs::create_dir_all(work.out()).unwrap();
        let fifo = std::ffi::CString::new(work.report().to_str().unwrap()).unwrap();
        // SAFETY: a valid, NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o666) }, 0);
        assert!(unreadable(report_promptly(&scratch.0)).contains("a pipe"));

        fs::remove_file(work.report()).unwrap();
        std::os::unix::fs::symlink("/dev/zero", work.report()).unwrap();
        assert!(unreadable(report_promptly(&scratch.0)).contains("a link"));

        let elsewhere = Scratch::new("hostile-report-elsewhere");
        let forged = Work::new(&elsewhere.0);
        fs::create_dir_all(forged.out()).unwrap();
        fs::write(forged.report(), b"{}").unwrap();
        fs::remove_file(work.report()).unwrap();
        fs::remove_dir(work.out()).unwrap();
        std::os::unix::fs::symlink(forged.out(), work.out()).unwrap();
        assert!(unreadable(report_promptly(&scratch.0)).contains("a link"));

        fs::remove_file(work.out()).unwrap();
        fs::create_dir_all(work.out()).unwrap();
        let big = fs::File::create(work.report()).unwrap();
        big.set_len(REPORT_LIMIT + 1).unwrap();
        assert!(unreadable(report_promptly(&scratch.0)).contains("larger than"));
    }

    #[test]
    fn the_build_root_is_made_for_its_owner_alone() {
        let scratch = Scratch::new("root");
        let root = scratch.0.join("above/builds");
        make_root(&root).unwrap();
        assert_eq!(mode(&root), 0o700);
        assert_eq!(reach(&root).unwrap(), Reach::Owner);
        // One that is there already is left as it is, and said to be open.
        set_mode(&root, 0o755).unwrap();
        make_root(&root).unwrap();
        assert_eq!(reach(&root).unwrap(), Reach::Others { mode: 0o755 });
        set_mode(&root, 0o500).unwrap();
        assert_eq!(reach(&root).unwrap(), Reach::Owner);
        set_mode(&root, 0o700).unwrap();
    }

    #[test]
    fn removing_what_is_not_there_is_done() {
        let scratch = Scratch::new("remove");
        let dir = scratch.0.join("7");
        fs::create_dir_all(dir.join("pkg")).unwrap();
        assert!(matches!(remove(&dir), Removal::Removed));
        assert!(!dir.exists());
        assert!(matches!(remove(&dir), Removal::Removed));
    }
}
