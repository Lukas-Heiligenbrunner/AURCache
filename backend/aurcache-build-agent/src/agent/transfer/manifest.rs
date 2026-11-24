//! Say what the build left: its report, and each package's name and size.

use aurcache_build_agent::layout::{OUT_DIR, PKG_DIR, REPORT_FILE, Work};
use aurcache_build_agent::report::{Artifact, BuildReport, Handback};
use aurcache_build_agent::spec::INTERFACE;
use aurcache_common::beneath::Dir;
use std::io::{self, Write};

/// The most a report may be. It is a few lines; anything near this is not one.
const REPORT_LIMIT: u64 = 1024 * 1024;

/// Print the [`Handback`] for `work` as one line of JSON.
pub fn run(work: &Work) -> io::Result<()> {
    let root = Dir::open(work.root())?;
    let handback = Handback {
        interface: INTERFACE,
        report: report(&root)?,
        artifacts: artifacts(&root)?,
    };
    let mut line = serde_json::to_vec(&handback)?;
    line.push(b'\n');
    let mut out = io::stdout().lock();
    out.write_all(&line)?;
    out.flush()
}

/// The build's report; `None` when there is none, or when what is there is
/// not a regular file in a directory of its own, which the agent did not
/// leave.
fn report(root: &Dir) -> io::Result<Option<BuildReport>> {
    let Ok(bytes) = root
        .dir(OUT_DIR)
        .and_then(|out| out.read(REPORT_FILE, REPORT_LIMIT))
    else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// The regular files in `pkg/`, sorted by name. A `pkg/` that is not a
/// directory of its own is refused, and so is a file that cannot be read:
/// leaving it out would lose a package.
fn artifacts(root: &Dir) -> io::Result<Vec<Artifact>> {
    let pkg = match root.dir(PKG_DIR) {
        Ok(pkg) => pkg,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut artifacts = Vec::new();
    for name in pkg.names()? {
        let Ok(name) = name.into_string() else {
            continue;
        };
        let file = match pkg.file(&name) {
            Ok(file) => file,
            // Not a regular file, which `Dir` refuses as such.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => continue,
            Err(error) => return Err(error),
        };
        let size = file.metadata()?.len();
        artifacts.push(Artifact { name, size });
    }
    artifacts.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(artifacts)
}

#[cfg(test)]
mod tests {
    use super::{artifacts, report};
    use aurcache_build_agent::layout::Work;
    use aurcache_build_agent::report::Artifact;
    use aurcache_common::beneath::Dir;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn only_regular_files_are_listed_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        let pkg = work.pkg();
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("b-1-1-any.pkg.tar.zst"), b"hello").unwrap();
        std::fs::write(pkg.join("a-1-1-any.pkg.tar.zst"), b"").unwrap();
        std::fs::create_dir(pkg.join("dir-1-1-any.pkg.tar.zst")).unwrap();
        symlink("/etc/passwd", pkg.join("link-1-1-any.pkg.tar.zst")).unwrap();
        let artifact = |name: &str, size| Artifact {
            name: name.to_string(),
            size,
        };
        assert_eq!(
            artifacts(&Dir::open(dir.path()).unwrap()).unwrap(),
            [
                artifact("a-1-1-any.pkg.tar.zst", 0),
                artifact("b-1-1-any.pkg.tar.zst", 5),
            ]
        );
    }

    #[test]
    fn a_package_that_cannot_be_read_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = Work::new(dir.path()).pkg();
        std::fs::create_dir(&pkg).unwrap();
        let unreadable = pkg.join("a-1-1-any.pkg.tar.zst");
        std::fs::write(&unreadable, b"").unwrap();
        std::fs::set_permissions(&unreadable, PermissionsExt::from_mode(0o000)).unwrap();
        if std::fs::File::open(&unreadable).is_ok() {
            return; // root reads it anyway
        }
        let error = artifacts(&Dir::open(dir.path()).unwrap()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
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
        assert!(
            report(&Dir::open(elsewhere.path()).unwrap())
                .unwrap()
                .is_some()
        );

        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        symlink(real.pkg(), work.pkg()).unwrap();
        symlink(real.out(), work.out()).unwrap();
        let root = Dir::open(dir.path()).unwrap();
        assert!(artifacts(&root).is_err());
        assert!(report(&root).unwrap().is_none());
    }

    #[test]
    fn what_is_missing_is_empty_and_a_report_that_is_not_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        let root = Dir::open(dir.path()).unwrap();
        assert!(artifacts(&root).unwrap().is_empty());
        assert!(report(&root).unwrap().is_none());
        std::fs::create_dir(work.out()).unwrap();
        std::fs::write(work.report(), b"{").unwrap();
        assert!(report(&root).is_err());
    }
}
