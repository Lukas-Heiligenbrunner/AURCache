//! Say what the build left: its report and each package's name, size and digest.

use super::file::{Opened, open_regular, pump};
use anyhow::{Context, Result};
use aurcache_build_agent::layout::Work;
use aurcache_build_agent::report::{Artifact, BuildReport, Handback};
use aurcache_build_agent::spec::INTERFACE;
use aurcache_common::artifact::is_artifact;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

/// The most a report may be. It is a few lines; anything near this is not one.
const REPORT_LIMIT: u64 = 16 * 1024 * 1024;

/// Print the [`Handback`] for `work` as one line of JSON.
pub fn run(work: &Work) -> Result<()> {
    let handback = Handback {
        interface: INTERFACE,
        report: report(&work.report())?,
        artifacts: artifacts(&work.pkg())?,
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
/// The agent writes a regular file. Anything else at its place (a pipe that
/// would never be read to its end, a link to somewhere else) was put there
/// by something else, and is no report.
fn report(path: &Path) -> Result<Option<BuildReport>> {
    let file = match open_regular(path) {
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

/// The packages in `dir`, by name: its regular files whose names are a
/// package's. Anything else there is not listed, and so is never sent.
fn artifacts(dir: &Path) -> Result<Vec<Artifact>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("cannot list {}", dir.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let name = entry.with_context(|| format!("cannot list {}", dir.display()))?;
        if let Ok(name) = name.file_name().into_string()
            && is_artifact(&name)
        {
            names.push(name);
        }
    }
    names.sort();
    let mut artifacts = Vec::new();
    for name in names {
        if let Some(artifact) = describe(dir, name)? {
            artifacts.push(artifact);
        }
    }
    Ok(artifacts)
}

/// `name`'s size and digest, or `None` if it is not a regular file.
fn describe(dir: &Path, name: String) -> Result<Option<Artifact>> {
    let path = dir.join(&name);
    let Opened::Regular(mut file) =
        open_regular(&path).with_context(|| format!("cannot open {}", path.display()))?
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
    use std::os::unix::fs::symlink;

    #[test]
    fn only_regular_files_with_package_names_are_listed_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        std::fs::write(pkg.join("b-1-1-any.pkg.tar.zst"), b"hello").unwrap();
        std::fs::write(pkg.join("a-1-1-any.pkg.tar.zst"), b"").unwrap();
        std::fs::write(pkg.join("a-1-1-any.pkg.tar.zst.sig"), b"sig").unwrap();
        std::fs::write(pkg.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir(pkg.join("dir-1-1-any.pkg.tar.zst")).unwrap();
        symlink("/etc/passwd", pkg.join("link-1-1-any.pkg.tar.zst")).unwrap();
        let listed = artifacts(pkg).unwrap();
        let shown: Vec<_> = listed
            .iter()
            .map(|a| (a.name.as_str(), a.size, a.sha256.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "a-1-1-any.pkg.tar.zst",
                    0,
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                ),
                (
                    "b-1-1-any.pkg.tar.zst",
                    5,
                    "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
                ),
            ]
        );
    }

    #[test]
    fn a_missing_directory_or_report_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(artifacts(&dir.path().join("pkg")).unwrap().is_empty());
        assert!(report(&dir.path().join("report.json")).unwrap().is_none());
    }

    #[test]
    fn a_report_that_is_not_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");
        std::fs::write(&path, b"{").unwrap();
        assert!(report(&path).is_err());
    }
}
