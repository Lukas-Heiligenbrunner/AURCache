//! The last step: the report, then the directory back to whoever gave it.
//!
//! Both happen once every other process is dead, so nothing the build
//! started can write the report or change a package after it was measured.
//! That is also what makes it safe for root to walk `pkg/`, which the build
//! user could write: files are still opened without following a link and
//! changed through the handle.

use super::dirs::{self, Owner};
use aurcache_build_agent::layout::Work;
use aurcache_build_agent::report::BuildReport;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, fchown};
use std::path::Path;

/// The report while it is being written, beside where it will be.
const PARTIAL: &str = ".report.json.partial";

/// What is done with what is not a regular file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Others {
    /// Left as they are.
    Keep,
    /// Removed when they are not directories: a link, or anything else
    /// that is not a file. Nothing but files belongs there, and what the
    /// build user made is not the owner's to remove. A directory, empty or
    /// not, is left, never walked: a tree as deep as the build can make
    /// would run this process's stack out. It goes with the work directory,
    /// which whoever gave it removes.
    Remove,
}

/// Write the report where the worker reads it.
///
/// It appears whole or not at all: a worker that waits for the file never
/// reads half of it. Root in the container (a dependency's scriptlet) could
/// have left anything at either name, so the partial file is made anew,
/// never opened: a pipe there would hold the open up for ever. The rename
/// replaces whatever is at the report's place.
pub fn write(work: &Work, report: &BuildReport) -> io::Result<()> {
    let partial = work.out().join(PARTIAL);
    match std::fs::remove_file(&partial) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&partial)?;
    file.write_all(&serde_json::to_vec_pretty(report)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&partial, work.report())
}

/// Give the work directory, and `pkg/`, `log/` and `out/` with the regular
/// files in them, to `owner`, readable by anyone.
///
/// Best effort: a worker that cannot read what it is given says so itself.
pub fn give(work: &Work, owner: Owner) {
    give_dir(&work.pkg(), owner, Others::Remove);
    if let Some(log) = work.log().parent() {
        give_dir(log, owner, Others::Keep);
    }
    give_dir(&work.out(), owner, Others::Keep);
    if let Ok(root) = dirs::open(work.root()) {
        let _ = fchown(&root, Some(owner.uid), Some(owner.gid));
    }
}

/// Give one directory and the regular files directly in it.
fn give_dir(path: &Path, owner: Owner, others: Others) {
    let Ok(dir) = dirs::open(path) else { return };
    for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file() {
            if let Ok(file) = open_file(&path) {
                give_handle(&file, owner, 0o644);
            }
        } else if others == Others::Remove && !meta.is_dir() {
            let _ = std::fs::remove_file(&path);
        }
    }
    give_handle(&dir, owner, 0o755);
}

/// Give an open file or directory to `owner` with `mode`.
///
/// Where root may not give it away, as under a runtime that maps root to
/// someone who owns nothing else, it is made writable by anyone instead, so
/// that whoever owns the directory can still clear it.
fn give_handle(file: &File, owner: Owner, mode: u32) {
    let mode = match fchown(file, Some(owner.uid), Some(owner.gid)) {
        Ok(()) => mode,
        Err(_) => mode | 0o666,
    };
    let _ = file.set_permissions(Permissions::from_mode(mode));
}

/// Open a regular file itself, never a link to one, and never waiting on
/// anything that is not a file.
fn open_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(io::Error::other("not a regular file"))
    }
}

#[cfg(test)]
mod tests {
    use super::{Others, give_dir, write};
    use crate::agent::build::dirs::Owner;
    use aurcache_build_agent::layout::Work;
    use aurcache_build_agent::report::{BuildReport, Ended};
    use aurcache_build_agent::spec::INTERFACE;
    use std::collections::BTreeMap;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn own() -> Owner {
        let meta = std::fs::metadata("/proc/self").unwrap();
        Owner {
            uid: meta.uid(),
            gid: meta.gid(),
        }
    }

    #[test]
    fn the_report_is_written_whole_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let work = Work::new(tmp.path());
        std::fs::create_dir(work.out()).unwrap();
        let report = BuildReport {
            interface: INTERFACE,
            ended: Ended::Exited { code: 0 },
            peak_memory_bytes: Some(1 << 20),
            oom_kills: Some(0),
            disk_bytes: None,
            vcs_commits: BTreeMap::new(),
        };
        write(&work, &report).unwrap();
        let read: BuildReport =
            serde_json::from_slice(&std::fs::read(work.report()).unwrap()).unwrap();
        assert_eq!(read, report);
        let names: Vec<_> = std::fs::read_dir(work.out())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["report.json"]);
        let mode = std::fs::metadata(work.report()).unwrap().mode();
        assert_eq!(mode & 0o444, 0o444, "{mode:o}");
    }

    /// What root in the container may have left at the two names, a pipe
    /// at each, neither holds the report up nor stays.
    #[test]
    fn the_report_replaces_what_was_left_at_its_place() {
        let tmp = tempfile::tempdir().unwrap();
        let work = Work::new(tmp.path());
        std::fs::create_dir(work.out()).unwrap();
        for path in [work.out().join(super::PARTIAL), work.report()] {
            let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            // SAFETY: a valid, NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o666) }, 0);
        }
        let report = BuildReport {
            interface: INTERFACE,
            ended: Ended::Exited { code: 0 },
            peak_memory_bytes: None,
            oom_kills: None,
            disk_bytes: None,
            vcs_commits: BTreeMap::new(),
        };
        let (send, receive) = std::sync::mpsc::channel();
        let writing = work.clone();
        let written = report.clone();
        std::thread::spawn(move || send.send(write(&writing, &written).is_ok()));
        let wrote = receive
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the report is written without waiting");
        assert!(wrote);
        assert!(std::fs::symlink_metadata(work.report()).unwrap().is_file());
        let read: BuildReport =
            serde_json::from_slice(&std::fs::read(work.report()).unwrap()).unwrap();
        assert_eq!(read, report);
    }

    /// A link in `pkg/` is never followed: what it points at keeps its mode,
    /// and the link itself goes. A directory, empty or not, is left as it
    /// is, not walked.
    #[test]
    fn only_regular_files_are_given_and_the_rest_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("pkg");
        let secret = tmp.path().join("secret");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(&secret, "x").unwrap();
        std::fs::set_permissions(&secret, PermissionsExt::from_mode(0o600)).unwrap();
        std::fs::write(pkg.join("a-1-1-any.pkg.tar.zst"), "x").unwrap();
        std::fs::set_permissions(
            pkg.join("a-1-1-any.pkg.tar.zst"),
            PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        symlink(&secret, pkg.join("b-1-1-any.pkg.tar.zst")).unwrap();
        std::fs::create_dir(pkg.join("empty")).unwrap();
        std::fs::create_dir_all(pkg.join("dir/deeper")).unwrap();
        std::fs::write(pkg.join("dir/deeper/file"), "x").unwrap();

        give_dir(&pkg, own(), Others::Remove);

        let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().mode() & 0o777;
        assert_eq!(mode(&pkg.join("a-1-1-any.pkg.tar.zst")), 0o644);
        assert_eq!(mode(&secret), 0o600);
        assert_eq!(mode(&pkg), 0o755);
        assert!(std::fs::symlink_metadata(pkg.join("b-1-1-any.pkg.tar.zst")).is_err());
        assert!(pkg.join("empty").is_dir());
        assert!(pkg.join("dir/deeper/file").is_file());

        // Elsewhere, what is not a file stays.
        std::fs::create_dir(pkg.join("kept")).unwrap();
        give_dir(&pkg, own(), Others::Keep);
        assert!(pkg.join("kept").is_dir());
    }
}
