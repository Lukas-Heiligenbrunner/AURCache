//! Source extraction and built-artifact discovery.
//!
//! Both ends of a build are executor-independent: the server always ships the
//! same patched `tar.gz`, and a build always ends as `*.pkg.tar.*` files in a
//! directory, however it was produced.

use crate::beneath;
use anyhow::{Context, Result};
use std::fs::File;
use std::io;
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

/// A built package, open for reading.
#[derive(Debug)]
pub struct Artifact {
    /// Its name in the directory, one [`is_artifact`] accepts.
    pub name: String,
    pub file: File,
    /// Its size when it was opened.
    pub len: u64,
}

/// The built packages in `dir` (`*.pkg.tar.*`, excluding detached `.sig`
/// signatures, unfinished `.part` files and hidden files), each open, in
/// the order of their names. Nothing when `dir` is not there.
///
/// The directory holds whatever the PKGBUILD's source and build put there,
/// and in a container, whatever root in it did. So `dir` must be a directory
/// itself, not a link to one, and only regular files in it are packages,
/// opened without following a link: a link named like a package, or a `dir`
/// that is one, would otherwise be uploaded as what it points at -- the
/// worker's own key, its build credential, any file of the worker's host.
///
/// # Errors
///
/// When `dir` is not a directory of its own, or cannot be read.
pub fn discover_artifacts(dir: &Path) -> io::Result<Vec<Artifact>> {
    let dir = match beneath::Dir::open(dir) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut names: Vec<String> = dir
        .names()?
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| is_artifact(name))
        .collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        // Anything that is not a regular file of the directory's own is no
        // package.
        let Ok(file) = dir.file(&name) else { continue };
        let len = file.metadata()?.len();
        out.push(Artifact { name, file, len });
    }
    Ok(out)
}

/// True for a built package artifact filename.
///
/// The character allow-list matters: these names come from whatever the
/// PKGBUILD wrote into the build directory and are then used as a URL path
/// segment and a repo-tree filename. Restricting to the set alpm actually
/// produces (`pkgname-pkgver-pkgrel-arch.pkg.tar.*`, where epoch contributes
/// `:` and pkgver may contain `+`, `.`, `_`, `~`) keeps a hostile or merely
/// broken PKGBUILD from smuggling separators or traversal through either.
#[must_use]
pub fn is_artifact(name: &str) -> bool {
    !name.starts_with('.')
        && name.contains(".pkg.tar")
        && !name.ends_with(".sig")
        && !name.ends_with(".part")
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+' | ':' | '~' | '@')
        })
}

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
        let real = "demo-1-1-x86_64.pkg.tar.zst";
        std::fs::write(dir.path().join(real), "package").unwrap();
        std::os::unix::fs::symlink(&key, dir.path().join("leak-1-1-x86_64.pkg.tar.zst")).unwrap();

        assert_eq!(names(&discover_artifacts(dir.path()).unwrap()), [real]);
    }

    /// Nor is a directory that is a link: every regular file in what it
    /// points at, a directory of the worker's host, would be a package.
    #[test]
    fn a_package_directory_that_is_a_link_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret-1-1-x86_64.pkg.tar.zst"), "host").unwrap();
        let work = tempfile::tempdir().unwrap();
        let pkg = work.path().join("pkg");
        std::os::unix::fs::symlink(outside.path(), &pkg).unwrap();

        let refused = discover_artifacts(&pkg).unwrap_err();
        assert!(refused.to_string().contains("a link"), "{refused}");
    }

    /// A pipe or a directory named like a package is skipped, without
    /// waiting on the pipe.
    #[test]
    fn only_regular_files_are_packages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("d-1-1-any.pkg.tar.zst")).unwrap();
        let fifo =
            std::ffi::CString::new(dir.path().join("p-1-1-any.pkg.tar.zst").to_str().unwrap())
                .unwrap();
        // SAFETY: a valid, NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        std::fs::write(dir.path().join("f-1-1-any.pkg.tar.zst"), "pkg").unwrap();

        let found = discover_artifacts(dir.path()).unwrap();
        assert_eq!(names(&found), ["f-1-1-any.pkg.tar.zst"]);
        assert_eq!(found[0].len, 3);
    }

    #[test]
    fn a_directory_that_is_not_there_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            discover_artifacts(&dir.path().join("pkg"))
                .unwrap()
                .is_empty()
        );
    }

    fn names(found: &[Artifact]) -> Vec<&str> {
        found.iter().map(|a| a.name.as_str()).collect()
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
        let found = discover_artifacts(dir.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "hello-1.0-1-x86_64.pkg.tar.zst");
    }

    #[test]
    fn is_artifact_rules() {
        assert!(is_artifact("a-1-1-x86_64.pkg.tar.zst"));
        assert!(is_artifact("a-1-1-x86_64.pkg.tar.xz"));
        assert!(!is_artifact("a-1-1-x86_64.pkg.tar.zst.sig"));
        assert!(!is_artifact(".x.pkg.tar.zst"));
        assert!(!is_artifact("PKGBUILD"));
    }

    /// Real alpm version syntax must survive the character allow-list.
    #[test]
    fn is_artifact_accepts_epoch_and_pkgver_punctuation() {
        assert!(is_artifact("foo-2:1.0_beta+3~rc1-1-x86_64.pkg.tar.zst"));
        assert!(is_artifact("lib32-gcc-libs-14.2-1-x86_64.pkg.tar.zst"));
    }

    /// Names that would corrupt the upload URL or escape the repo directory.
    #[test]
    fn is_artifact_rejects_url_and_path_hostile_names() {
        assert!(!is_artifact("evil?x=1.pkg.tar.zst"));
        assert!(!is_artifact("evil#frag.pkg.tar.zst"));
        assert!(!is_artifact("../../etc/passwd.pkg.tar.zst"));
        assert!(!is_artifact("a b-1-1-x86_64.pkg.tar.zst"));
        assert!(!is_artifact("a%2f-1-1-x86_64.pkg.tar.zst"));
    }
}
