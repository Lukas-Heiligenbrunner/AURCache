//! What a build used: memory, from the container's own cgroup, and disk.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Where the unified cgroup hierarchy is mounted.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// What the container's cgroup recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    /// The most memory the container held at once.
    pub peak_bytes: Option<u64>,
    /// How many times the kernel killed a process in it for memory.
    pub oom_kills: Option<u64>,
}

/// Read the memory figures of the cgroup this process is in.
///
/// The cgroup is the one `/proc/self/cgroup` names, under the mounted
/// hierarchy: in a container with its own cgroup namespace that is the mount
/// itself, and in one that shares the host's it is the container's own
/// directory, not the host's root.
pub fn memory() -> Memory {
    let Some(dir) = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| own_cgroup(&text))
    else {
        return Memory::default();
    };
    let read = |file: &str| std::fs::read_to_string(dir.join(file)).ok();
    Memory {
        peak_bytes: read("memory.peak").and_then(|text| text.trim().parse().ok()),
        oom_kills: read("memory.events").and_then(|text| oom_kills(&text)),
    }
}

/// The bytes on disk under the work directory and the scratch directory, or
/// `None` when any part of either could not be read: a sum of what could
/// would read as a figure, and be wrong.
pub fn disk(work: &Path, scratch: &Path) -> Option<u64> {
    let mut seen = HashSet::new();
    let mut total = under(work, &mut seen)?;
    if !scratch.starts_with(work) {
        total = total.checked_add(under(scratch, &mut seen)?)?;
    }
    Some(total)
}

/// The directory of this process's cgroup in the unified hierarchy.
fn own_cgroup(proc_self_cgroup: &str) -> Option<PathBuf> {
    let path = proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?;
    // A path that climbs out of the namespace's root is a cgroup this mount
    // does not show.
    let inside = path.strip_prefix('/')?;
    if inside.split('/').any(|part| part == "..") {
        return None;
    }
    Some(Path::new(CGROUP_ROOT).join(inside))
}

/// The `oom_kill` count of a `memory.events` file.
fn oom_kills(events: &str) -> Option<u64> {
    events.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some("oom_kill"))
            .then(|| fields.next()?.parse().ok())
            .flatten()
    })
}

/// The bytes allocated to a tree, links not followed, a file with several
/// names counted once.
fn under(root: &Path, seen: &mut HashSet<(u64, u64)>) -> Option<u64> {
    let mut total = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if meta.nlink() > 1 && !meta.is_dir() && !seen.insert((meta.dev(), meta.ino())) {
            continue;
        }
        total = total.checked_add(meta.blocks().checked_mul(512)?)?;
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path).ok()? {
                pending.push(entry.ok()?.path());
            }
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::{disk, oom_kills, own_cgroup};
    use std::path::Path;

    #[test]
    fn the_cgroup_is_the_one_the_process_is_in() {
        // A container with a cgroup namespace of its own.
        assert_eq!(
            own_cgroup("0::/\n").as_deref(),
            Some(Path::new("/sys/fs/cgroup"))
        );
        // One that shares the host's: its own directory, not the root.
        assert_eq!(
            own_cgroup("0::/machine.slice/libpod-abc.scope/container\n").as_deref(),
            Some(Path::new(
                "/sys/fs/cgroup/machine.slice/libpod-abc.scope/container"
            ))
        );
        // The first hierarchy only: nothing to read on a host without it.
        assert_eq!(
            own_cgroup("12:memory:/docker/abc\n1:name=systemd:/docker/abc\n"),
            None
        );
        // A cgroup outside what the namespace shows.
        assert_eq!(own_cgroup("0::/../other\n"), None);
        assert_eq!(own_cgroup(""), None);
    }

    #[test]
    fn oom_kills_are_read_from_their_own_line() {
        let events = "low 0\nhigh 0\nmax 12\noom 3\noom_kill 2\noom_group_kill 1\n";
        assert_eq!(oom_kills(events), Some(2));
        assert_eq!(oom_kills("low 0\noom_group_kill 4\n"), None);
        assert_eq!(oom_kills("oom_kill many\n"), None);
    }

    #[test]
    fn disk_counts_each_tree_once() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        let inner = work.join("scratch");
        let outer = tmp.path().join("scratch");
        for dir in [&inner, &outer] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(inner.join("a"), vec![1u8; 300_000]).unwrap();
        std::fs::write(outer.join("b"), vec![1u8; 300_000]).unwrap();
        // A second name for the same file, and a link that leads outside.
        std::fs::hard_link(inner.join("a"), work.join("again")).unwrap();
        std::os::unix::fs::symlink(&outer, work.join("away")).unwrap();

        let nested = disk(&work, &inner).unwrap();
        assert!((300_000..600_000).contains(&nested), "{nested}");
        let apart = disk(&work, &outer).unwrap();
        assert!((600_000..900_000).contains(&apart), "{apart}");
        assert_eq!(disk(&work, &tmp.path().join("missing")), None);
    }
}
