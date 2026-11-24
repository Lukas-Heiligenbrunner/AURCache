//! Say what the build left: its report, its log's length, and each package's
//! name and size. What cannot be read is left out: root in the build container
//! could have put anything at these names.

use aurcache_build_agent::layout::{LOG_DIR, LOG_FILE, OUT_DIR, PKG_DIR, REPORT_FILE, Work};
use aurcache_build_agent::report::{Artifact, BuildReport, Handback, REPORT_LIMIT};
use aurcache_build_agent::spec::INTERFACE;
use aurcache_common::artifact::is_artifact;
use aurcache_common::beneath::Dir;
use std::ffi::OsString;
use std::io::{self, Write};

/// Print the [`Handback`] for `work` as one line of JSON.
pub fn run(work: &Work) -> io::Result<()> {
    let root = Dir::open(work.root())?;
    let handback = Handback {
        interface: INTERFACE,
        report: report(&root),
        log_size: log_size(&root),
        artifacts: artifacts(&root),
    };
    let mut line = serde_json::to_vec(&handback)?;
    line.push(b'\n');
    let mut out = io::stdout().lock();
    out.write_all(&line)?;
    out.flush()
}

/// The build's report; `None` unless `out` is a directory, not a link, and
/// `out/report.json` is a regular file in it that parses as a report.
fn report(root: &Dir) -> Option<BuildReport> {
    let bytes = root
        .dir(OUT_DIR)
        .and_then(|out| out.read(REPORT_FILE, REPORT_LIMIT))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The length of the build log, if there is one that can be read.
fn log_size(root: &Dir) -> Option<u64> {
    root.dir(LOG_DIR)
        .and_then(|dir| dir.file(LOG_FILE))
        .and_then(|file| file.metadata())
        .ok()
        .map(|metadata| metadata.len())
}

/// The packages in `pkg/`: regular files that can be opened, named as the
/// workers accept, sorted by name.
fn artifacts(root: &Dir) -> Vec<Artifact> {
    let Ok(pkg) = root.dir(PKG_DIR) else {
        return Vec::new();
    };
    // Filtered as they are read: `pkg/` can hold millions of other names.
    let listed: io::Result<Vec<Artifact>> = pkg.names().and_then(|names| {
        names
            .filter_map(|name| name.map(|name| artifact(&pkg, name)).transpose())
            .collect()
    });
    let mut artifacts = listed.unwrap_or_default();
    artifacts.sort_by(|a, b| a.name.cmp(&b.name));
    artifacts
}

/// `name` in `pkg` as a package, if the workers accept the name and it is a
/// regular file that can be opened.
fn artifact(pkg: &Dir, name: OsString) -> Option<Artifact> {
    let name = name.into_string().ok().filter(|name| is_artifact(name))?;
    let size = pkg.file(&name).and_then(|file| file.metadata()).ok()?.len();
    Some(Artifact { name, size })
}

#[cfg(test)]
mod tests {
    use super::{artifacts, log_size, report};
    use aurcache_build_agent::layout::Work;
    use aurcache_build_agent::report::Artifact;
    use aurcache_common::beneath::Dir;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn only_regular_package_files_are_listed_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        let pkg = work.pkg();
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("b-1-1-any.pkg.tar.zst"), b"hello").unwrap();
        std::fs::write(pkg.join("a-1-1-any.pkg.tar.zst"), b"").unwrap();
        std::fs::write(pkg.join("a\x01-1-1-any.pkg.tar.zst"), b"").unwrap();
        std::fs::create_dir(pkg.join("dir-1-1-any.pkg.tar.zst")).unwrap();
        symlink("/etc/passwd", pkg.join("link-1-1-any.pkg.tar.zst")).unwrap();
        let artifact = |name: &str, size| Artifact {
            name: name.to_string(),
            size,
        };
        assert_eq!(
            artifacts(&Dir::open(dir.path()).unwrap()),
            [
                artifact("a-1-1-any.pkg.tar.zst", 0),
                artifact("b-1-1-any.pkg.tar.zst", 5),
            ]
        );
    }

    #[test]
    fn a_package_that_cannot_be_read_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = Work::new(dir.path()).pkg();
        std::fs::create_dir(&pkg).unwrap();
        let unreadable = pkg.join("a-1-1-any.pkg.tar.zst");
        std::fs::write(&unreadable, b"").unwrap();
        std::fs::set_permissions(&unreadable, PermissionsExt::from_mode(0o000)).unwrap();
        if std::fs::File::open(&unreadable).is_ok() {
            return; // root reads it anyway
        }
        assert!(artifacts(&Dir::open(dir.path()).unwrap()).is_empty());
    }

    /// A `pkg/` or an `out/` that is a link leads nowhere: what it points at
    /// is not this build's.
    #[test]
    fn a_linked_directory_is_not_followed() {
        let elsewhere = tempfile::tempdir().unwrap();
        let real = Work::new(elsewhere.path());
        std::fs::create_dir_all(real.pkg()).unwrap();
        std::fs::create_dir_all(real.out()).unwrap();
        std::fs::write(
            real.report(),
            br#"{"interface":1,"ended":{"how":"exited","code":0}}"#,
        )
        .unwrap();
        assert!(report(&Dir::open(elsewhere.path()).unwrap()).is_some());

        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        symlink(real.pkg(), work.pkg()).unwrap();
        symlink(real.out(), work.out()).unwrap();
        let root = Dir::open(dir.path()).unwrap();
        assert!(artifacts(&root).is_empty());
        assert!(report(&root).is_none());
    }

    #[test]
    fn what_is_missing_or_not_a_report_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        let root = Dir::open(dir.path()).unwrap();
        assert!(artifacts(&root).is_empty());
        assert!(report(&root).is_none());
        assert_eq!(log_size(&root), None);
        std::fs::create_dir(work.log().parent().unwrap()).unwrap();
        std::fs::write(work.log(), b"a log").unwrap();
        assert_eq!(log_size(&root), Some(5));
        std::fs::create_dir(work.out()).unwrap();
        std::fs::write(work.report(), b"{").unwrap();
        assert!(report(&root).is_none());
    }
}
