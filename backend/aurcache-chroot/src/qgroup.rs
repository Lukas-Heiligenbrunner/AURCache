//! Quota groups: their identifiers, the fixed ones this pool uses, and their
//! usage, read from sysfs.
//!
//! btrfs publishes every qgroup's counters under
//! `/sys/fs/btrfs/<fsid>/qgroups/<level>_<id>/`, world-readable, so reading a
//! build's usage needs neither root nor a tree walk.

use std::fmt;
use std::path::{Path, PathBuf};

/// A qgroup, `level/id`. Level 0 is a subvolume's own group, its id the
/// subvolume id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QgroupId {
    pub level: u16,
    pub id: u64,
}

/// The pool's total: everything the worker stores is under it. btrfs only
/// lets a group's parent be at a *higher* level, so the total is the one
/// level-2 group and everything else hangs below it.
pub const TOTAL: QgroupId = QgroupId { level: 2, id: 0 };

/// Level-1 ids below this are reserved for the caches' own groups; a build's
/// group is `1/(BUILD_BASE + build id)`.
pub const BUILD_BASE: u64 = 1000;

impl QgroupId {
    #[must_use]
    pub const fn subvolume(id: u64) -> Self {
        Self { level: 0, id }
    }

    /// The group one build's subvolumes are gathered in.
    #[must_use]
    pub fn build(build_id: i32) -> Self {
        let id = u64::try_from(build_id).expect("build ids are positive");
        Self {
            level: 1,
            id: BUILD_BASE + id,
        }
    }

    /// The build this group belongs to, if it is a build's group.
    #[must_use]
    pub fn build_id(self) -> Option<i32> {
        (self.level == 1 && self.id >= BUILD_BASE)
            .then(|| i32::try_from(self.id - BUILD_BASE).ok())
            .flatten()
    }

    /// The directory name sysfs gives this group.
    fn sysfs_name(self) -> String {
        format!("{}_{}", self.level, self.id)
    }

    fn from_sysfs_name(name: &str) -> Option<Self> {
        let (level, id) = name.split_once('_')?;
        Some(Self {
            level: level.parse().ok()?,
            id: id.parse().ok()?,
        })
    }
}

impl fmt::Display for QgroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.level, self.id)
    }
}

/// What a group has charged to it, and its limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    /// Bytes charged. Under simple quotas, an extent is charged to the
    /// subvolume that wrote it, and that is what this counts.
    pub used: u64,
    /// The limit, `None` when unlimited.
    pub limit: Option<u64>,
}

impl Usage {
    /// Whether the group has reached its limit, within `slack` bytes: a write
    /// that failed for want of quota leaves the group just short of it.
    #[must_use]
    pub fn at_limit(self, slack: u64) -> bool {
        self.limit
            .is_some_and(|limit| self.used.saturating_add(slack) >= limit)
    }
}

/// The qgroups of one mounted filesystem, as sysfs shows them.
#[derive(Clone, Debug)]
pub struct Qgroups {
    dir: PathBuf,
}

impl Qgroups {
    /// The qgroups of the filesystem with this UUID.
    #[must_use]
    pub fn for_fsid(fsid: &str) -> Self {
        Self::at(Path::new("/sys/fs/btrfs").join(fsid).join("qgroups"))
    }

    /// For tests: a directory laid out as sysfs lays it out.
    #[must_use]
    pub const fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `squota`, `qgroup` or `disabled`; `None` if quotas were never enabled.
    #[must_use]
    pub fn mode(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("mode"))
            .ok()
            .map(|m| m.trim().to_string())
    }

    #[must_use]
    pub fn exists(&self, group: QgroupId) -> bool {
        self.dir.join(group.sysfs_name()).is_dir()
    }

    /// A group's usage, `None` if the group does not exist.
    #[must_use]
    pub fn usage(&self, group: QgroupId) -> Option<Usage> {
        let dir = self.dir.join(group.sysfs_name());
        let read = |name: &str| -> Option<u64> {
            std::fs::read_to_string(dir.join(name))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        let used = read("referenced")?;
        // `0` is how sysfs says "no limit".
        let limit = read("max_referenced").filter(|&l| l > 0);
        Some(Usage { used, limit })
    }

    /// Every group there is.
    #[must_use]
    pub fn list(&self) -> Vec<QgroupId> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut groups: Vec<_> = entries
            .flatten()
            .filter_map(|e| QgroupId::from_sysfs_name(&e.file_name().to_string_lossy()))
            .collect();
        groups.sort_unstable();
        groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_group_round_trips_to_its_build() {
        let group = QgroupId::build(42);
        assert_eq!(group.to_string(), "1/1042");
        assert_eq!(group.build_id(), Some(42));
        assert_eq!(
            QgroupId { level: 1, id: 3 }.build_id(),
            None,
            "a cache group"
        );
        assert_eq!(QgroupId::subvolume(1042).build_id(), None, "a subvolume");
    }

    #[test]
    fn sysfs_names_are_level_underscore_id() {
        let group = QgroupId { level: 2, id: 0 };
        assert_eq!(group.sysfs_name(), "2_0");
        assert_eq!(QgroupId::from_sysfs_name("2_0"), Some(group));
        assert_eq!(QgroupId::from_sysfs_name("mode"), None);
        assert_eq!(QgroupId::from_sysfs_name("drop_subtree_threshold"), None);
    }

    #[test]
    fn usage_reads_what_sysfs_publishes() {
        let dir = std::env::temp_dir().join(format!("qgroups-{}", std::process::id()));
        let group = dir.join("1_1042");
        std::fs::create_dir_all(&group).unwrap();
        std::fs::write(group.join("referenced"), "10502144\n").unwrap();
        std::fs::write(group.join("max_referenced"), "52428800\n").unwrap();
        std::fs::create_dir_all(dir.join("0_256")).unwrap();
        std::fs::write(dir.join("0_256/referenced"), "16384\n").unwrap();
        std::fs::write(dir.join("0_256/max_referenced"), "0\n").unwrap();
        std::fs::write(dir.join("mode"), "squota\n").unwrap();

        let qgroups = Qgroups::at(dir.clone());
        assert_eq!(qgroups.mode().as_deref(), Some("squota"));
        assert_eq!(
            qgroups.usage(QgroupId::build(42)),
            Some(Usage {
                used: 10_502_144,
                limit: Some(52_428_800)
            })
        );
        assert_eq!(
            qgroups.usage(QgroupId::subvolume(256)).unwrap().limit,
            None,
            "0 means unlimited"
        );
        assert_eq!(qgroups.usage(QgroupId::build(7)), None);
        assert_eq!(
            qgroups.list(),
            [QgroupId::subvolume(256), QgroupId::build(42)]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn at_limit_allows_for_the_write_that_did_not_fit() {
        let usage = |used| Usage {
            used,
            limit: Some(100 << 20),
        };
        assert!(usage(100 << 20).at_limit(0));
        assert!(usage((100 << 20) - 4096).at_limit(1 << 20));
        assert!(!usage(50 << 20).at_limit(1 << 20));
        assert!(
            !Usage {
                used: u64::MAX,
                limit: None
            }
            .at_limit(0)
        );
    }
}
