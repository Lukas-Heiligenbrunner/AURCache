//! Fixtures for the tests that publish packages. The API's upload tests
//! include this file too.
#![allow(dead_code)]

use std::io::Read;
use std::path::{Path, PathBuf};

/// Build logs go somewhere disposable, the same place for every test in this
/// process: the server reads it from the environment, so tests running side
/// by side keep their logs apart by each building a pkgbase of its own.
pub fn quiet_logs() {
    static LOGS: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        // SAFETY: set once, before any test reads it, and never changed.
        unsafe { std::env::set_var("AURCACHE_BUILD_LOG_PATH", &dir) };
        dir
    });
}

/// A package file, as makepkg names it and writes its `.PKGINFO`.
pub struct Pkg<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub arch: &'a str,
}

pub fn pkg<'a>(name: &'a str, version: &'a str, arch: &'a str) -> Pkg<'a> {
    Pkg {
        name,
        version,
        arch,
    }
}

impl Pkg<'_> {
    pub fn filename(&self) -> String {
        format!("{}-{}-{}.pkg.tar.zst", self.name, self.version, self.arch)
    }

    pub fn bytes(&self) -> Vec<u8> {
        pkg_zst(&format!(
            "pkgname = {}\npkgver = {}\narch = {}\n",
            self.name, self.version, self.arch
        ))
    }
}

/// A `.pkg.tar.zst` holding only `pkginfo`, as its `.PKGINFO`.
pub fn pkg_zst(pkginfo: &str) -> Vec<u8> {
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_path(".PKGINFO").unwrap();
        header.set_size(pkginfo.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, pkginfo.as_bytes()).unwrap();
        builder.finish().unwrap();
    }
    zstd::stream::encode_all(tar_bytes.as_slice(), 0).unwrap()
}

/// The entry directories `repo.db` lists for `platform` under the repository
/// `root`, or none if it has none.
pub fn listed(root: &Path, platform: &str) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(root.join(platform).join("repo.db.tar.gz")) else {
        return Vec::new();
    };
    let mut data = Vec::new();
    file.read_to_end(&mut data).unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(data.as_slice()));
    let mut dirs: Vec<String> = archive
        .entries()
        .unwrap()
        .flatten()
        .filter(|e| e.header().entry_type().is_dir())
        .map(|e| e.path().unwrap().display().to_string())
        .collect();
    dirs.sort();
    dirs
}
