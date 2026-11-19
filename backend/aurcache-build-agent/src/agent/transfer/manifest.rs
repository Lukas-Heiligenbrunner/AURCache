//! Say what the build left: its report and each package's name, size and digest.

use super::file::{Beneath, Opened, OpenedDir, open_dir_in, pump};
use anyhow::{Context, Result, bail};
use aurcache_build_agent::layout::{OUT_DIR, PKG_DIR, REPORT_FILE, Work};
use aurcache_build_agent::report::{Artifact, BuildReport, Handback};
use aurcache_build_agent::spec::INTERFACE;
use aurcache_common::artifact::is_artifact;
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::path::Path;

/// The most a report may be. It is a few lines; anything near this is not one.
const REPORT_LIMIT: u64 = 16 * 1024 * 1024;

/// Print the [`Handback`] for `work` as one line of JSON.
pub fn run(work: &Work) -> Result<()> {
    let handback = Handback {
        interface: INTERFACE,
        report: report(work)?,
        artifacts: artifacts(work)?,
    };
    let mut line = serde_json::to_vec(&handback).context("cannot encode the manifest")?;
    line.push(b'\n');
    let mut out = io::stdout().lock();
    out.write_all(&line)
        .and_then(|()| out.flush())
        .context("cannot write the manifest")
}

/// The build's report, or `None` when the build wrote none.
///
/// The agent writes a regular file in a directory of its own. Anything else
/// at either place (a pipe that would never be read to its end, a link to
/// somewhere else) was put there by something else, and is no report.
fn report(work: &Work) -> Result<Option<BuildReport>> {
    let path = work.report();
    let file = match open_dir_in(work.root(), OUT_DIR).and_then(|out| match out {
        OpenedDir::Dir(out) => out.regular(REPORT_FILE),
        OpenedDir::Other => Ok(Opened::Other),
    }) {
        Ok(Opened::Regular(file)) => file,
        Ok(Opened::Other) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot open {}", path.display())),
    };
    let mut text = Vec::new();
    file.take(REPORT_LIMIT)
        .read_to_end(&mut text)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let report = serde_json::from_slice(&text)
        .with_context(|| format!("{} is not a build report", path.display()))?;
    Ok(Some(report))
}

/// The packages in `<work>/pkg`, by name: its regular files whose names are
/// a package's. Anything else there is not listed, and so is never sent. A
/// `pkg/` that is not a directory of its own (a link to elsewhere, which is
/// not followed) is refused.
fn artifacts(work: &Work) -> Result<Vec<Artifact>> {
    let dir = work.pkg();
    let pkg = match open_dir_in(work.root(), PKG_DIR) {
        Ok(OpenedDir::Dir(pkg)) => pkg,
        Ok(OpenedDir::Other) => bail!("{} is not a directory", dir.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("cannot open {}", dir.display())),
    };
    let mut names: Vec<String> = pkg
        .names()
        .with_context(|| format!("cannot list {}", dir.display()))?
        .into_iter()
        .filter(|name| is_artifact(name))
        .collect();
    names.sort();
    let mut artifacts = Vec::new();
    for name in names {
        if let Some(artifact) = describe(&pkg, &dir, name)? {
            artifacts.push(artifact);
        }
    }
    Ok(artifacts)
}

/// `name`'s size and digest, or `None` if it is not a regular file. `dir`
/// is where `pkg` is, for messages.
fn describe(pkg: &Beneath, dir: &Path, name: String) -> Result<Option<Artifact>> {
    let path = dir.join(&name);
    let Opened::Regular(mut file) = pkg
        .regular(&name)
        .with_context(|| format!("cannot open {}", path.display()))?
    else {
        return Ok(None);
    };
    let mut hash = Hashing::default();
    pump(&mut file, &mut hash).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(Some(Artifact {
        name,
        size: hash.bytes,
        sha256: hex::encode(hash.digest.finalize()),
    }))
}

/// A sink that counts and hashes what is written to it, and keeps none of it.
#[derive(Default)]
struct Hashing {
    digest: Sha256,
    bytes: u64,
}

impl Write for Hashing {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.digest.update(data);
        self.bytes += data.len() as u64;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{artifacts, report};
    use aurcache_build_agent::layout::Work;
    use aurcache_build_agent::report::Artifact;
    use std::os::unix::fs::symlink;

    fn artifact(name: &str, size: u64, sha256: &str) -> Artifact {
        Artifact {
            name: name.to_string(),
            size,
            sha256: sha256.to_string(),
        }
    }

    #[test]
    fn only_regular_files_with_package_names_are_listed_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        let pkg = work.pkg();
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("b-1-1-any.pkg.tar.zst"), b"hello").unwrap();
        std::fs::write(pkg.join("a-1-1-any.pkg.tar.zst"), b"").unwrap();
        std::fs::write(pkg.join("a-1-1-any.pkg.tar.zst.sig"), b"sig").unwrap();
        std::fs::write(pkg.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir(pkg.join("dir-1-1-any.pkg.tar.zst")).unwrap();
        symlink("/etc/passwd", pkg.join("link-1-1-any.pkg.tar.zst")).unwrap();
        assert_eq!(
            artifacts(&work).unwrap(),
            [
                artifact(
                    "a-1-1-any.pkg.tar.zst",
                    0,
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                ),
                artifact(
                    "b-1-1-any.pkg.tar.zst",
                    5,
                    "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
                ),
            ]
        );
    }

    /// A `pkg/` or an `out/` that is a link leads nowhere: the packages and
    /// the report it points at are not this build's.
    #[test]
    fn a_linked_directory_is_not_followed() {
        let elsewhere = tempfile::tempdir().unwrap();
        let real = Work::new(elsewhere.path());
        std::fs::create_dir_all(real.pkg()).unwrap();
        std::fs::write(real.pkg().join("a-1-1-any.pkg.tar.zst"), b"").unwrap();
        std::fs::create_dir_all(real.out()).unwrap();
        std::fs::write(
            real.report(),
            br#"{"interface":1,"ended":{"how":"exited","code":0},"peak_memory_bytes":null,"oom_kills":null,"disk_bytes":null,"vcs_commits":{}}"#,
        )
        .unwrap();
        assert!(report(&real).unwrap().is_some());

        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        symlink(real.pkg(), work.pkg()).unwrap();
        symlink(real.out(), work.out()).unwrap();
        let refused = artifacts(&work).unwrap_err().to_string();
        assert!(refused.contains("is not a directory"), "{refused}");
        assert!(report(&work).unwrap().is_none());
    }

    #[test]
    fn a_missing_directory_or_report_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        assert!(artifacts(&work).unwrap().is_empty());
        assert!(report(&work).unwrap().is_none());
        std::fs::create_dir(work.out()).unwrap();
        assert!(report(&work).unwrap().is_none());
    }

    #[test]
    fn a_report_that_is_not_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        std::fs::create_dir(work.out()).unwrap();
        std::fs::write(work.report(), b"{").unwrap();
        assert!(report(&work).is_err());
    }
}
