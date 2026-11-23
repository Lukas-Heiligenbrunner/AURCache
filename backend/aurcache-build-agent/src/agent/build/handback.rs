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
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, fchown};
use std::path::Path;

/// The report while it is being written, beside where it will be.
const PARTIAL: &str = ".report.json.partial";

/// Write the report where the worker reads it.
///
/// It appears whole or not at all: a worker that waits for the file never
/// reads half of it. `out/` is made anew first, since root in the container
/// (a dependency's scriptlet) could have left anything at it or in it.
/// What is there must be the empty directory the build was given: anything
/// else, and there is no report, so the build is handed back as interrupted.
pub fn write(work: &Work, report: &BuildReport) -> io::Result<()> {
    let out = work.out();
    // Removes an empty directory, and nothing else: not a link, not a file.
    std::fs::remove_dir(&out)?;
    std::fs::DirBuilder::new().mode(0o755).create(&out)?;
    let partial = out.join(PARTIAL);
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

/// Give the work directory, and `pkg/` and `out/` with the regular files in
/// them, to `owner`, readable by anyone.
///
/// Best effort: a worker that cannot read what it is given says so itself.
pub fn give(work: &Work, owner: Owner) {
    give_dir(&work.pkg(), owner);
    give_dir(&work.out(), owner);
    if let Ok(root) = dirs::open(work.root()) {
        let _ = fchown(&root, Some(owner.uid), Some(owner.gid));
    }
}

/// Give one directory and the regular files directly in it. Nothing else in
/// it is followed or walked: the worker reads only regular files there.
fn give_dir(path: &Path, owner: Owner) {
    let Ok(dir) = dirs::open(path) else { return };
    for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
        if let Ok(file) = open_file(&entry.path()) {
            give_handle(&file, owner, 0o644);
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
    use super::{give_dir, write};
    use crate::agent::build::dirs::Owner;
    use aurcache_build_agent::layout::Work;
    use aurcache_build_agent::report::{BuildReport, Ended};
    use aurcache_build_agent::spec::INTERFACE;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn own() -> Owner {
        let meta = std::fs::metadata("/proc/self").unwrap();
        Owner {
            uid: meta.uid(),
            gid: meta.gid(),
        }
    }

    fn report() -> BuildReport {
        BuildReport {
            interface: INTERFACE,
            ended: Ended::Exited { code: 0 },
        }
    }

    #[test]
    fn the_report_is_written_whole_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let work = Work::new(tmp.path());
        std::fs::create_dir(work.out()).unwrap();
        write(&work, &report()).unwrap();
        let read: BuildReport =
            serde_json::from_slice(&std::fs::read(work.report()).unwrap()).unwrap();
        assert_eq!(read, report());
        let names: Vec<_> = std::fs::read_dir(work.out())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["report.json"]);
        let mode = std::fs::metadata(work.report()).unwrap().mode();
        assert_eq!(mode & 0o444, 0o444, "{mode:o}");
    }

    /// Whatever root in the container left at `out/` or in it, a link or a
    /// pipe among them, means no report, at once, and nothing a link points
    /// at is written to.
    #[test]
    fn what_root_left_at_out_means_no_report() {
        let elsewhere = tempfile::tempdir().unwrap();
        let plants: [fn(&Work, &std::path::Path); 3] = [
            |work, _| std::fs::write(work.out(), "x").unwrap(),
            |work, elsewhere| symlink(elsewhere, work.out()).unwrap(),
            |work, _| {
                std::fs::create_dir(work.out()).unwrap();
                let name = std::ffi::CString::new(work.report().to_str().unwrap()).unwrap();
                // SAFETY: a valid, NUL-terminated path.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o666) }, 0);
            },
        ];
        for (i, plant) in plants.iter().enumerate() {
            let tmp = tempfile::tempdir().unwrap();
            let work = Work::new(tmp.path());
            plant(&work, elsewhere.path());
            assert!(write(&work, &report()).is_err(), "plant {i}");
        }
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    /// A link in `pkg/` is never followed: what it points at keeps its mode.
    /// Only the regular files are given; the rest is left as it is.
    #[test]
    fn only_regular_files_are_given() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("pkg");
        let secret = tmp.path().join("secret");
        let package = pkg.join("a-1-1-any.pkg.tar.zst");
        std::fs::create_dir(&pkg).unwrap();
        for file in [&secret, &package] {
            std::fs::write(file, "x").unwrap();
            std::fs::set_permissions(file, PermissionsExt::from_mode(0o600)).unwrap();
        }
        symlink(&secret, pkg.join("b-1-1-any.pkg.tar.zst")).unwrap();
        std::fs::create_dir_all(pkg.join("dir/deeper")).unwrap();

        give_dir(&pkg, own());

        let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().mode() & 0o777;
        assert_eq!(mode(&package), 0o644);
        assert_eq!(mode(&secret), 0o600);
        assert_eq!(mode(&pkg), 0o755);
        assert!(pkg.join("dir/deeper").is_dir());
    }
}
