//! Subvolumes for the entries of a long-lived cache in the pool: one per
//! package's sources, one per kept build tree.
//!
//! An entry that is a subvolume of its own is measured by its quota group --
//! a read of sysfs, where a directory has to be walked, sometimes millions of
//! files deep -- and removed by deleting the subvolume, which returns at once
//! where `rm -rf` over such a tree takes minutes. And a per-package limit,
//! should one ever be wanted, is a qgroup limit on it.
//!
//! Blocking, unlike the rest of the crate: the cache code that uses it
//! already runs on blocking threads, walking and deleting.

use crate::cmd::{privileged_blocking, query_blocking};
use crate::pool::Owner;
use crate::qgroup::{QgroupId, Qgroups, TOTAL};
use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// btrfs, from `statfs`.
const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;

/// The inode number of every btrfs subvolume's root, and only of those.
const SUBVOLUME_ROOT_INODE: u64 = 256;

/// The entries of one cache subvolume in the pool.
///
/// Only acts on paths strictly inside that subvolume: an entry is named by
/// the cache code, but deleting a subvolume is root's, and never reaches
/// outside the cache it was handed.
#[derive(Clone, Debug)]
pub struct CacheVolumes {
    root: PathBuf,
    qgroups: Qgroups,
}

impl CacheVolumes {
    pub(crate) fn new(root: PathBuf, qgroups: Qgroups) -> Self {
        Self { root, qgroups }
    }

    /// The cache subvolume these entries live in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Make `path` a subvolume counted against the pool's total, if nothing
    /// is there yet; directories above it are made as plain directories.
    /// Something already there, subvolume or not, is left as it is.
    pub fn ensure(&self, path: &Path, owner: Owner, mode: u32) -> Result<()> {
        self.check_inside(path)?;
        if path.symlink_metadata().is_ok() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        privileged_blocking(&[
            "btrfs".as_ref(),
            "subvolume".as_ref(),
            "create".as_ref(),
            path.as_os_str(),
        ])?;
        let made = (|| {
            privileged_blocking(&[
                "chown".as_ref(),
                owner.chown_spec().as_ref(),
                path.as_os_str(),
            ])?;
            privileged_blocking(&[
                "chmod".as_ref(),
                format!("{mode:o}").as_ref(),
                path.as_os_str(),
            ])?;
            let id = subvolume_id(path)?;
            // Its own writes are charged to its own group, which counts toward
            // nothing until it is under the total. Made inside the cache
            // subvolume, which is under the total, the kernel may already
            // have put it there (it did on 7.2): that answers `File exists`.
            let assigned = privileged_blocking(&[
                "btrfs".as_ref(),
                "qgroup".as_ref(),
                "assign".as_ref(),
                "--no-rescan".as_ref(),
                QgroupId::subvolume(id).to_string().as_ref(),
                TOTAL.to_string().as_ref(),
                self.root.as_os_str(),
            ]);
            match assigned {
                Ok(_) => {}
                Err(e) if format!("{e:#}").contains("File exists") => {}
                Err(e) => return Err(e),
            }
            anyhow::Ok(())
        })();
        if let Err(e) = made {
            // Never left in place uncounted: the next call would take it as is.
            let _ = self.delete(path);
            return Err(e).with_context(|| format!("preparing {}", path.display()));
        }
        Ok(())
    }

    /// Whether `path` is a subvolume -- an entry made by [`Self::ensure`] --
    /// rather than a plain directory from before entries were subvolumes.
    #[must_use]
    pub fn is_volume(&self, path: &Path) -> bool {
        self.check_inside(path).is_ok() && is_subvolume(path)
    }

    /// Bytes charged to the entry at `path`; `None` when it is not a
    /// subvolume, and has to be measured some other way.
    #[must_use]
    pub fn usage(&self, path: &Path) -> Option<u64> {
        if !self.is_volume(path) {
            return None;
        }
        let id = subvolume_id(path).ok()?;
        self.qgroups
            .usage(QgroupId::subvolume(id))
            .map(|usage| usage.used)
    }

    /// Delete the subvolume at `path`, and everything in it. Returns at once;
    /// btrfs frees the space in the background. Refused for anything that is
    /// not a subvolume inside this cache.
    pub fn remove(&self, path: &Path) -> Result<()> {
        if !self.is_volume(path) {
            bail!("{} is not a cache subvolume", path.display());
        }
        self.delete(path)
    }

    fn delete(&self, path: &Path) -> Result<()> {
        privileged_blocking(&[
            "btrfs".as_ref(),
            "subvolume".as_ref(),
            "delete".as_ref(),
            // A build could make subvolumes of its own below its tree.
            "--recursive".as_ref(),
            path.as_os_str(),
        ])
        .map(drop)
    }

    fn check_inside(&self, path: &Path) -> Result<()> {
        let inside = path.starts_with(&self.root)
            && path != self.root
            && !path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
        if !inside {
            bail!(
                "{} is not inside the cache at {}",
                path.display(),
                self.root.display()
            );
        }
        Ok(())
    }
}

/// Whether `path` is itself a subvolume: on btrfs, and its root inode. Needs
/// no privilege, unlike `btrfs subvolume show`. A symlink is never one.
fn is_subvolume(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = path.symlink_metadata() else {
        return false;
    };
    meta.is_dir() && meta.ino() == SUBVOLUME_ROOT_INODE && fs_type(path) == Some(BTRFS_SUPER_MAGIC)
}

pub(crate) fn fs_type(path: &Path) -> Option<i64> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: `c_path` is NUL-terminated and outlives the call; `buf` is a
    // valid out-parameter.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &raw mut buf) } != 0 {
        return None;
    }
    // `as`: the field's type differs between targets.
    #[allow(clippy::unnecessary_cast)]
    Some(buf.f_type as i64)
}

fn subvolume_id(path: &Path) -> Result<u64> {
    let out = query_blocking(&[
        OsStr::new("btrfs"),
        "inspect-internal".as_ref(),
        "rootid".as_ref(),
        path.as_os_str(),
    ])?;
    out.trim()
        .parse()
        .with_context(|| format!("subvolume id of {}: {out:?}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volumes() -> CacheVolumes {
        CacheVolumes::new(
            PathBuf::from("/pool/cache"),
            Qgroups::at(PathBuf::from("/nonexistent")),
        )
    }

    /// Deleting a subvolume is root's, so the paths it accepts are fenced to
    /// the cache it was handed.
    #[test]
    fn only_paths_strictly_inside_the_cache_are_accepted() {
        let v = volumes();
        assert!(
            v.check_inside(Path::new("/pool/cache/srcdest/hello"))
                .is_ok()
        );
        assert!(
            v.check_inside(Path::new("/pool/cache")).is_err(),
            "the cache itself"
        );
        assert!(v.check_inside(Path::new("/pool/root")).is_err());
        assert!(v.check_inside(Path::new("/pool/cache/../root")).is_err());
        assert!(v.check_inside(Path::new("/pool/cachefoo/x")).is_err());
    }

    #[test]
    fn a_plain_directory_is_not_a_subvolume() {
        let dir = std::env::temp_dir();
        assert!(!is_subvolume(&dir.join("definitely-not-here-aurcache")));
    }
}
