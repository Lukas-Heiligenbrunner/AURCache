//! The job's source archive, unpacked by the build user.
//!
//! The archive is what the server sent: a PKGBUILD's directory, under one
//! directory named after the package base. Its entries are not root's to
//! write anywhere, so root only copies the archive to where the build user
//! can read it, and the build user extracts it with `bsdtar`, which pacman's
//! libarchive puts in every image this runs in.

use super::run;
use super::user::BuildUser;
use aurcache_build_agent::layout::{Scratch, Work};
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// The copy of the archive the build user reads, in the scratch directory.
const COPY: &str = "source.tar.gz";

/// Unpack the job's source archive into the scratch directory, and check it
/// held the package's directory. `root` is the scratch directory itself,
/// which is root's.
pub fn unpack(
    work: &Work,
    root: &Path,
    scratch: &Scratch,
    pkgbase: &str,
    user: &BuildUser,
) -> Result<(), String> {
    let copy = root.join(COPY);
    std::fs::copy(work.source(), &copy)
        .and_then(|_| std::fs::set_permissions(&copy, Permissions::from_mode(0o644)))
        .map_err(|e| format!("the source archive could not be read: {e}"))?;
    let mut bsdtar = user.command("bsdtar");
    bsdtar.arg("-xzf").arg(&copy).arg("-C").arg(scratch.src());
    let status = run::logged(&mut bsdtar)
        .map_err(|e| format!("the source archive could not be unpacked: {e}"))?;
    let _ = std::fs::remove_file(&copy);
    if !status.success() {
        return Err(format!(
            "unpacking the source archive {}",
            run::describe(status)
        ));
    }
    match std::fs::symlink_metadata(scratch.startdir(pkgbase)) {
        Ok(meta) if meta.is_dir() => Ok(()),
        _ => Err(format!("the source archive has no `{pkgbase}` directory")),
    }
}
