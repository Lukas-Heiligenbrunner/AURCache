//! Filesystem helpers shared by the server and the standalone worker.
//!
//! Behind the `fs` feature, which browser consumers leave off along with `db`.

use std::fs;
use std::path::{Path, PathBuf};

/// Whether every regular file in a directory tree could be measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryMeasurement {
    Complete { bytes: u64 },
    Incomplete { known_bytes: u64 },
}

impl DirectoryMeasurement {
    /// The total only when every entry could be read.
    #[must_use]
    pub const fn total(self) -> Option<u64> {
        match self {
            Self::Complete { bytes } => Some(bytes),
            Self::Incomplete { .. } => None,
        }
    }

    /// The measured lower bound, including when part of the tree was unreadable.
    #[must_use]
    pub const fn known_bytes(self) -> u64 {
        match self {
            Self::Complete { bytes } => bytes,
            Self::Incomplete { known_bytes } => known_bytes,
        }
    }
}

/// Measure regular files iteratively, without following symlinks.
///
/// Missing entries, unreadable directories and overflowing totals make the
/// result incomplete. They never turn an unknown total into zero.
#[must_use]
pub fn measure_directory(path: impl AsRef<Path>) -> DirectoryMeasurement {
    let mut total = 0_u64;
    let mut complete = true;
    let mut stack = vec![path.as_ref().to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(dir) = fs::read_dir(&path) else {
            complete = false;
            continue;
        };
        for entry in dir {
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            match entry.metadata() {
                Ok(data) if data.is_dir() => stack.push(entry.path()),
                Ok(data) if data.is_file() => match total.checked_add(data.len()) {
                    Some(bytes) => total = bytes,
                    None => {
                        total = u64::MAX;
                        complete = false;
                    }
                },
                Ok(_) => {}
                Err(_) => complete = false,
            }
        }
    }
    if complete {
        DirectoryMeasurement::Complete { bytes: total }
    } else {
        DirectoryMeasurement::Incomplete { known_bytes: total }
    }
}

/// Directory holding one log file per build.
///
/// Relative by default, like the repository and the source cache, so a
/// deployment that mounts a single data directory gets this inside it.
///
/// Lives here rather than beside the logger because the backfill migration in
/// `aurcache-db` needs it too, and `aurcache-db` cannot depend on the crate the
/// logger lives in without a cycle.
#[must_use]
pub fn build_log_root() -> PathBuf {
    std::env::var("AURCACHE_BUILD_LOG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./build_logs"))
}

/// Where the repository lives, relative to the server's working directory.
pub const REPO_ROOT: &str = "./repo";

/// Where the server keeps the internal CA that signs every worker's
/// certificate.
#[must_use]
pub fn ca_dir() -> PathBuf {
    std::env::var("AURCACHE_CA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data/ca"))
}

/// One path segment, with anything that is not a plain name flattened.
///
/// A pkgbase from the AUR cannot contain a separator, but `AURCache` also builds
/// from git, where the pkgbase comes from a parsed PKGBUILD and is whatever
/// that file says. `.` and `..` are handled separately because they survive
/// character filtering intact and are still traversal.
///
/// Distinct names could in principle collapse onto one segment; that needs a
/// pkgbase containing a character the AUR does not allow, and the cost is a
/// shared log directory rather than anything escaping the root.
fn sanitize_segment(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

/// Directory holding one package's build logs.
#[must_use]
pub fn build_log_dir(pkgbase: &str) -> PathBuf {
    build_log_root().join(sanitize_segment(pkgbase))
}

/// Path of one build's log, keyed the way the API is.
///
/// `<pkgbase>/<number>.log` rather than the build id: `<pkgbase>/<number>` is
/// a build's public identity -- what the URLs, the CLI and the screens all use
/// -- and the id is an internal key none of them show. Naming the files after
/// the identity means someone reading the directory sees what they see
/// everywhere else, and deleting a package's logs is one `remove_dir_all`.
#[must_use]
pub fn build_log_path(pkgbase: &str, number: i32) -> PathBuf {
    build_log_dir(pkgbase).join(format!("{number}.log"))
}

/// Atomically replace a credential file, owner-only on Unix.
///
/// The sibling temporary file is synced before replacement. Readers see a
/// complete old or new file; symlinks are replaced, never followed. A parent
/// sync failure is reported after replacement. The parent must already exist.
pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Temporary(PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (temporary, mut file) = loop {
        let temporary = parent.join(format!(
            ".private-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => break (Temporary(temporary), file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary.0, path)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain tree sums its files recursively. Directories contribute only
    /// what is inside them, never their own inode size.
    #[test]
    fn plain_tree_sums_all_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        fs::write(tmp.path().join("one"), [b'1'; 10]).unwrap();
        fs::write(tmp.path().join("a/b/two"), [b'2'; 20]).unwrap();
        assert_eq!(measure_directory(tmp.path()).total(), Some(30));
    }

    /// A directory symlink is counted as one entry, not traversed — the
    /// property the worker's cache sizing relies on. Uses a 1 MB marker file:
    /// if `link` were followed, the total would include its contents twice.
    #[cfg(unix)]
    #[test]
    fn directory_symlink_is_counted_once_not_traversed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("real")).unwrap();
        fs::write(tmp.path().join("real/data"), vec![b'x'; 1_000_000]).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();

        let total = measure_directory(tmp.path()).known_bytes();
        assert!(
            total < 1_000_000 + 2_000,
            "symlinked directory contents leaked into total: {total}"
        );
    }

    /// Depth nobody controls must not become stack depth. Paths cap nesting
    /// around 1500 levels (`PATH_MAX`), which a recursive walk survives on a
    /// normal stack — so this runs the walk on a 64 KiB thread stack, where
    /// recursion per level overflows and the explicit stack does not.
    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dir = tmp.path().to_path_buf();
        for _ in 0..1500 {
            dir.push("d");
        }
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("deep"), [b'x'; 7]).unwrap();
        let root = tmp.path().to_path_buf();
        let total = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || measure_directory(&root))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(total.total(), Some(7));
    }

    /// A symlink loop terminates: each symlink is one entry, counted once and
    /// never re-entered.
    #[cfg(unix)]
    #[test]
    fn symlink_loop_terminates() {
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(tmp.path().join("b"), tmp.path().join("a")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("a"), tmp.path().join("b")).unwrap();

        let total = measure_directory(tmp.path()).known_bytes();
        assert!(
            total < 4096,
            "loop should be counted once, not resolved: {total}"
        );
    }

    /// The best-effort contract: an unreadable subtree costs only itself. The
    /// readable sibling is still counted, rather than the whole walk collapsing
    /// to zero the way a `Result` plus `unwrap_or(0)` used to make it.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_subtree_does_not_zero_the_total() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("readable")).unwrap();
        fs::write(tmp.path().join("readable/data"), [b'x'; 1000]).unwrap();

        let locked = tmp.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("hidden"), [b'y'; 1000]).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        // Running as root defeats the permission bits, so `locked` may or may
        // not be counted; what must hold either way is that `readable` is.
        let total = measure_directory(tmp.path()).known_bytes();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            total >= 1000,
            "a failed subtree swallowed its readable sibling: {total}"
        );
    }

    /// A missing directory is zero, not a panic — the stats page asks for
    /// `repo/` before anything has been built.
    #[test]
    fn a_missing_directory_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            measure_directory(tmp.path().join("nope")),
            DirectoryMeasurement::Incomplete { known_bytes: 0 }
        );
    }
}

#[cfg(test)]
mod private_file_tests {
    use super::*;
    #[test]
    fn replacement_is_private_and_failed_replacement_keeps_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential");
        fs::write(&path, "old").unwrap();
        write_private(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let directory = dir.path().join("directory");
        fs::create_dir(&directory).unwrap();
        assert!(write_private(&directory, b"bad").is_err());
        assert!(directory.is_dir());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}
