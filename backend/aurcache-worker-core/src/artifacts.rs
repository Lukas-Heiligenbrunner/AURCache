//! Source extraction and built-artifact discovery.
//!
//! Both ends of a build are executor-independent: the server always ships the
//! same patched `tar.gz`, and a build always ends as `*.pkg.tar.*` files in a
//! directory, however it was produced.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Extract a `tar.gz` source archive (top-level `{pkgbase}/…`) into `dest` and
/// return the path to the extracted package directory.
pub fn extract_source(archive: &[u8], dest: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(dest).context("unpacking source archive")?;

    // The archive has a single top-level pkgbase directory.
    std::fs::read_dir(dest)
        .context("reading extracted source")?
        .flatten()
        .find(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path())
        .context("source archive had no package directory")
}

/// Find built package artifacts (`*.pkg.tar.*`, excluding detached `.sig`
/// signatures, unfinished `.part` files and hidden files) in a build directory.
#[must_use]
pub fn discover_artifacts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Regular files only, judged without following links: the directory
        // holds whatever the PKGBUILD's source and build put there, and a
        // symlink named like a package would otherwise be uploaded as the
        // file it points at -- the worker's own key, its build credential.
        let regular = entry.file_type().is_ok_and(|t| t.is_file());
        if regular && is_artifact(&name) {
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

pub use aurcache_common::artifact::is_artifact;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A symlink named like a package is not a package: it would be uploaded
    /// as whatever it points at, which on a worker includes its own key.
    #[test]
    fn a_symlink_named_like_a_package_is_not_an_artifact() {
        let outside = tempfile::tempdir().unwrap();
        let key = outside.path().join("worker-key.pem");
        std::fs::write(&key, "PRIVATE KEY").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("demo-1-1-x86_64.pkg.tar.zst");
        std::fs::write(&real, "package").unwrap();
        std::os::unix::fs::symlink(&key, dir.path().join("leak-1-1-x86_64.pkg.tar.zst")).unwrap();

        assert_eq!(discover_artifacts(dir.path()), [real]);
    }

    fn make_tar_gz(pkgbase: &str, files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            let mut tar = tar::Builder::new(enc);
            for (name, content) in files {
                let path = format!("{pkgbase}/{name}");
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(&mut header, path, content.as_bytes())
                    .unwrap();
            }
            tar.finish().unwrap();
        }
        buf
    }

    #[test]
    fn extracts_source_returns_pkgdir() {
        let dir = tempfile::tempdir().unwrap();
        let archive = make_tar_gz("hello", &[("PKGBUILD", "pkgname=hello")]);
        let pkgdir = extract_source(&archive, dir.path()).unwrap();
        assert_eq!(pkgdir.file_name().unwrap(), "hello");
        assert!(pkgdir.join("PKGBUILD").exists());
    }

    #[test]
    fn discovers_only_real_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "hello-1.0-1-x86_64.pkg.tar.zst",
            "hello-1.0-1-x86_64.pkg.tar.zst.sig",
            "hello-1.0-1-x86_64.pkg.tar.zst.part",
            "PKGBUILD",
            ".hidden.pkg.tar.zst",
        ] {
            let mut f = std::fs::File::create(dir.path().join(name)).unwrap();
            f.write_all(b"x").unwrap();
        }
        let found = discover_artifacts(dir.path());
        assert_eq!(found.len(), 1);
        assert!(
            found[0]
                .to_string_lossy()
                .ends_with("hello-1.0-1-x86_64.pkg.tar.zst")
        );
    }
}
