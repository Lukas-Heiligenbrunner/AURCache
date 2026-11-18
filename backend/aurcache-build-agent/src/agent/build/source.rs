//! The job's source archive, unpacked by the build user.
//!
//! The archive is what the server sent: a PKGBUILD's directory, under one
//! directory named after the package base. Its entries are not root's to
//! write anywhere, so root only copies the archive to where the build user
//! can read it, and the agent's own `unpack` subcommand, run as the build
//! user, extracts it.

use super::output::Output;
use super::run;
use super::user::BuildUser;
use aurcache_build_agent::layout::{Scratch, Work};
use flate2::read::GzDecoder;
use std::fs::{File, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::ExitCode;

/// The copy of the archive the build user reads, in the scratch directory.
const COPY: &str = "source.tar.gz";

/// What `unpack` needs to be run.
pub struct Unpacking<'a> {
    pub work: &'a Work,
    /// The scratch directory itself, which is root's.
    pub root: &'a Path,
    pub scratch: &'a Scratch,
    pub pkgbase: &'a str,
    /// This program's own path.
    pub agent: &'a Path,
}

/// Unpack the job's source archive into the scratch directory, and check it
/// held the package's directory.
pub fn unpack(unpacking: &Unpacking<'_>, user: &BuildUser, output: &Output) -> Result<(), String> {
    let copy = unpacking.root.join(COPY);
    std::fs::copy(unpacking.work.source(), &copy)
        .and_then(|_| std::fs::set_permissions(&copy, Permissions::from_mode(0o644)))
        .map_err(|e| format!("the source archive could not be read: {e}"))?;
    let mut command = user.command(unpacking.agent);
    command
        .arg("unpack")
        .arg("--archive")
        .arg(&copy)
        .arg("--into")
        .arg(unpacking.scratch.src());
    let status = run::logged(output, &mut command)
        .map_err(|e| format!("the source archive could not be unpacked: {e}"))?;
    let _ = std::fs::remove_file(&copy);
    if !status.success() {
        return Err(format!(
            "unpacking the source archive {}",
            run::describe(status)
        ));
    }
    let startdir = unpacking.scratch.startdir(unpacking.pkgbase);
    match std::fs::symlink_metadata(&startdir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        _ => Err(format!(
            "the source archive has no `{}` directory",
            unpacking.pkgbase
        )),
    }
}

/// The `unpack` subcommand: extract a gzipped tar into a directory. An entry
/// that would land outside the directory is refused by the extraction.
pub fn extract(archive: &Path, into: &Path) -> ExitCode {
    match try_extract(archive, into) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("aurcache-build-agent: unpacking {}: {e}", archive.display());
            ExitCode::FAILURE
        }
    }
}

fn try_extract(archive: &Path, into: &Path) -> io::Result<()> {
    let file = File::open(archive)?;
    tar::Archive::new(GzDecoder::new(file)).unpack(into)
}

#[cfg(test)]
mod tests {
    use super::try_extract;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::path::Path;

    /// A gzipped tar of the given entries, written without the checks a
    /// well-behaved archiver makes on names.
    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for (name, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder.append(&header, *content).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn unpacked(bytes: &[u8], into: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(into).unwrap();
        let file = into.parent().unwrap().join("source.tar.gz");
        std::fs::write(&file, bytes).unwrap();
        try_extract(&file, into)
    }

    #[test]
    fn a_package_directory_is_unpacked() {
        let tmp = tempfile::tempdir().unwrap();
        let into = tmp.path().join("src");
        let bytes = archive(&[
            ("hello/PKGBUILD", b"pkgname=hello\n"),
            ("hello/hello.c", b"int main(void) { return 0; }\n"),
        ]);
        unpacked(&bytes, &into).unwrap();
        assert_eq!(
            std::fs::read(into.join("hello/PKGBUILD")).unwrap(),
            b"pkgname=hello\n"
        );
        assert!(into.join("hello/hello.c").is_file());
    }

    #[test]
    fn nothing_is_written_outside_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let into = tmp.path().join("deep/src");
        let bytes = archive(&[("hello/PKGBUILD", b"x"), ("../escaped", b"x")]);
        let _ = unpacked(&bytes, &into);
        assert!(!tmp.path().join("deep/escaped").exists());
        assert!(!tmp.path().join("escaped").exists());
    }

    #[test]
    fn what_is_not_an_archive_fails() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(unpacked(b"not gzip at all", &tmp.path().join("src")).is_err());
        assert!(try_extract(&tmp.path().join("missing"), tmp.path()).is_err());
    }
}
