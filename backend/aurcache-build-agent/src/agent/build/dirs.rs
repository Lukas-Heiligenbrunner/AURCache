//! The directories of a build: whose they are while it runs.
//!
//! The work directory is root's from the first step to the last, so the
//! build user can create nothing in it and cannot rename what is there:
//! `out/`, where the report goes, stays what root made. The build user gets
//! `pkg/` and its own trees under the scratch directory, and nothing else.
//!
//! Everything here happens before the build user has a process, so nothing
//! can change under it. Directories are still opened without following a
//! link and changed through the handle, never by name.

use aurcache_build_agent::layout::{BUILD_GID, BUILD_UID, Scratch, Work};
use std::fs::{File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, fchown};
use std::path::Path;

/// Who a directory belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

pub const ROOT: Owner = Owner { uid: 0, gid: 0 };
pub const BUILD: Owner = Owner {
    uid: BUILD_UID,
    gid: BUILD_GID,
};

/// Open a directory itself: not what a link in its place points at.
pub fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

/// Make a directory, or take the one that is there, and give it an owner
/// and a mode.
pub fn make(path: &Path, owner: Owner, mode: u32) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(mode).create(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let dir = open(path)?;
    fchown(&dir, Some(owner.uid), Some(owner.gid))?;
    dir.set_permissions(Permissions::from_mode(mode))
}

/// Take the work directory for root, and return who it belonged to.
///
/// What matters is that the build user cannot write to it, which a runtime
/// that maps root to someone else may give without letting root own it. A
/// directory the build user could still write to is refused: a report in it
/// could not be told from one the build wrote.
pub fn take(work: &Work) -> io::Result<Owner> {
    let dir = open(work.root())?;
    let before = dir.metadata()?;
    let _ = fchown(&dir, Some(ROOT.uid), Some(ROOT.gid));
    let _ = dir.set_permissions(Permissions::from_mode(0o755));
    let now = dir.metadata()?;
    if !closed_to_build(now.uid(), now.gid(), now.mode()) {
        return Err(io::Error::other(format!(
            "{} could not be taken from the build user (owner {}:{}, mode {:o})",
            work.root().display(),
            now.uid(),
            now.gid(),
            now.mode() & 0o7777
        )));
    }
    make(&work.out(), ROOT, 0o755)?;
    // A report from before this build would be read as this build's.
    match std::fs::remove_file(work.report()) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    Ok(Owner {
        uid: before.uid(),
        gid: before.gid(),
    })
}

/// Open the build log for writing, empty, as a file of root's that anyone
/// may read.
pub fn log(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o644)
        .open(path)?;
    file.set_permissions(Permissions::from_mode(0o644))?;
    Ok(file)
}

/// Make the build's own trees, and `pkg/`, and give them to the build user.
pub fn lay_out(root: &Path, scratch: &Scratch, work: &Work) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    make(root, ROOT, 0o755)?;
    for tree in [scratch.src(), scratch.build(), scratch.srcdest()] {
        make(&tree, BUILD, 0o755)?;
    }
    make(&scratch.home(), BUILD, 0o700)?;
    make(&work.pkg(), BUILD, 0o755)
}

/// Whether a directory with this owner and mode is one the build user
/// cannot create, remove or rename anything in.
fn closed_to_build(uid: u32, gid: u32, mode: u32) -> bool {
    uid != BUILD_UID && mode & 0o002 == 0 && (gid != BUILD_GID || mode & 0o020 == 0)
}

#[cfg(test)]
mod tests {
    use super::{closed_to_build, open};
    use aurcache_build_agent::layout::{BUILD_GID, BUILD_UID};

    #[test]
    fn a_directory_the_build_user_can_write_is_not_closed() {
        assert!(closed_to_build(0, 0, 0o40755));
        // Someone else's, as under a runtime that will not let root own it.
        assert!(closed_to_build(65534, 65534, 0o40755));
        assert!(closed_to_build(0, BUILD_GID, 0o40755));
        assert!(closed_to_build(0, 0, 0o40775));
        // The build user's own, its group's with group write, anyone's.
        assert!(!closed_to_build(BUILD_UID, 0, 0o40755));
        assert!(!closed_to_build(BUILD_UID, 0, 0o40500));
        assert!(!closed_to_build(0, BUILD_GID, 0o40775));
        assert!(!closed_to_build(0, 0, 0o40757));
        assert!(!closed_to_build(0, 0, 0o41777));
    }

    #[test]
    fn a_link_to_a_directory_is_not_opened() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let link = tmp.path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(open(&real).is_ok());
        assert!(open(&link).is_err());
        std::fs::write(tmp.path().join("file"), "").unwrap();
        assert!(open(&tmp.path().join("file")).is_err());
    }
}
