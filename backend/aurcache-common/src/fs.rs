//! Filesystem helpers shared by the server and the standalone worker.
//!
//! Behind the `fs` feature, which browser consumers leave off along with `db`.

use std::fs;
use std::path::{Path, PathBuf};

mod hybrid;
pub use hybrid::{Adopted, Adoption, HybridDir, adopt_hybrid_dirs};

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
    if let Some(kept) = HybridDir::BuildLogs.in_repository().get() {
        return kept.clone();
    }
    configured_build_log_root()
}

fn configured_build_log_root() -> PathBuf {
    crate::server_env::AURCACHE_BUILD_LOG_PATH
        .get()
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./build_logs"))
}

/// Where the repository lives, relative to the server's working directory.
pub const REPO_ROOT: &str = "./repo";

/// Where the server keeps the internal CA that signs every worker's
/// certificate.
#[must_use]
pub fn ca_dir() -> PathBuf {
    if let Some(kept) = HybridDir::Ca.in_repository().get() {
        return kept.clone();
    }
    configured_ca_dir()
}

fn configured_ca_dir() -> PathBuf {
    crate::server_env::AURCACHE_CA_DIR
        .get()
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data/ca"))
}

/// Where the server keeps the SSH key it clones package sources with, and
/// the host keys it has trusted on first contact.
#[must_use]
pub fn ssh_dir() -> PathBuf {
    if let Some(kept) = HybridDir::Ssh.in_repository().get() {
        return kept.clone();
    }
    configured_ssh_dir()
}

fn configured_ssh_dir() -> PathBuf {
    crate::server_env::AURCACHE_SSH_DIR
        .get()
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data/ssh"))
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
    replace(path, contents, 0o600)
}

/// Atomically replace a file every user can read and only its owner can
/// change: `0644` on Unix, whatever the umask.
///
/// Replaced as [`write_private`] replaces a file: readers see a complete old
/// or new file, and a symlink at `path` is replaced, never followed.
pub fn write_public(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    replace(path, contents, 0o644)
}

/// Write `contents` to a new sibling of `path`, owner-only until it is
/// complete, give it `mode` and rename it over `path`.
#[cfg_attr(not(unix), allow(unused_variables))]
fn replace(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
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
    // On the open file, so the umask cannot narrow it and nothing at a path
    // can redirect it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary.0, path)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Keep what `path` holds beside it, before something replaces it: copied to
/// [`replaced_path`]`(path, tag)`, owner-only on Unix as [`write_private`]
/// writes it. A key or a certificate a restore replaces is then still there to
/// go back to. Where the copy went, or `None` when there is nothing at `path`.
///
/// `tag` says what is kept -- a fingerprint, a digest -- so that keeping the
/// same thing twice writes one file, and a reader can tell copies apart.
pub fn keep_replaced(path: &Path, tag: &str) -> std::io::Result<Option<PathBuf>> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let kept = replaced_path(path, tag)?;
    write_private(&kept, &contents)?;
    Ok(Some(kept))
}

/// Where [`keep_replaced`] keeps `path` under `tag`: beside it, as
/// `<name>.replaced-<tag>`. Refused for a tag that would lead out of the
/// directory, or is empty.
pub fn replaced_path(path: &Path, tag: &str) -> std::io::Result<PathBuf> {
    if tag.is_empty() || tag.contains(['/', '\\', '\0']) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{tag:?} cannot name a file"),
        ));
    }
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".replaced-{tag}"));
    Ok(PathBuf::from(name))
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Refuse `path` unless a rename can replace it: it is a regular file, or
/// nothing yet -- not a directory, not a symlink to something else, and not
/// mounted in place, as a Docker secret bind-mounted where the file would be
/// is, which a rename over it fails on (`EBUSY`).
///
/// For a change that has to know before it replaces anything that it can
/// replace all of it.
pub fn check_replaceable(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a regular file, which nothing here replaces",
            path.display()
        )));
    }
    if is_mount_point(path)? {
        return Err(std::io::Error::other(format!(
            "{} is mounted where it is, so it cannot be replaced; \
             mount it somewhere else and name it there",
            path.display()
        )));
    }
    Ok(())
}

/// Whether something is mounted at `path`, which exists: on another
/// filesystem than its directory, or listed as a mount point in
/// `/proc/self/mountinfo` -- a bind mount from the same filesystem keeps its
/// device, so only the kernel's table tells it.
fn is_mount_point(path: &Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = fs::metadata(parent_of(path))?;
        if fs::symlink_metadata(path)?.dev() != parent.dev() {
            return Ok(true);
        }
    }
    let Ok(table) = fs::read_to_string("/proc/self/mountinfo") else {
        return Ok(false);
    };
    let canonical = fs::canonicalize(path)?;
    Ok(mount_points(&table).any(|point| point == canonical))
}

/// The mount points `/proc/self/mountinfo` lists, its octal escapes undone.
fn mount_points(table: &str) -> impl Iterator<Item = PathBuf> + '_ {
    table
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(unescape_mount_point)
}

/// A mountinfo field as the path it names: the kernel writes a space, a
/// tab, a newline and a backslash as `\040`, `\011`, `\012` and `\134`.
fn unescape_mount_point(field: &str) -> PathBuf {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|digits| bytes[i] == b'\\' && digits.iter().all(|d| (b'0'..=b'7').contains(d)));
        match octal {
            Some(digits) => {
                let value = digits
                    .iter()
                    .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                out.push(u8::try_from(value).unwrap_or(b'?'));
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(out))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(&out).into_owned())
    }
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

        let total = measure_directory(tmp.path())
            .total()
            .expect("every entry is readable");
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

        let total = measure_directory(tmp.path())
            .total()
            .expect("every entry is readable");
        assert!(
            total < 4096,
            "loop should be counted once, not resolved: {total}"
        );
    }

    /// The best-effort contract: an unreadable subtree costs only itself. The
    /// readable sibling is still counted, rather than the whole walk collapsing
    /// to zero.
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
        let total = match measure_directory(tmp.path()) {
            DirectoryMeasurement::Complete { bytes } => bytes,
            DirectoryMeasurement::Incomplete { known_bytes } => known_bytes,
        };
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

    /// A public file is readable by everyone, and a symlink where it goes is
    /// replaced by it: what the link pointed at is left as it was.
    #[cfg(unix)]
    #[test]
    fn a_public_file_replaces_a_symlink_and_leaves_its_target_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, "kept").unwrap();
        let path = dir.path().join("public");
        std::os::unix::fs::symlink(&target, &path).unwrap();

        write_public(&path, b"new").unwrap();

        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_file());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read(&target).unwrap(), b"kept");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    /// What is replaced is kept beside it, private, under the tag it is
    /// given; nothing is kept of nothing, and a tag cannot leave the
    /// directory.
    #[test]
    fn a_replaced_file_is_kept_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id_ed25519");
        fs::write(&path, "old key").unwrap();
        let kept = keep_replaced(&path, "abc").unwrap().unwrap();
        assert_eq!(kept, dir.path().join("id_ed25519.replaced-abc"));
        assert_eq!(fs::read(&kept).unwrap(), b"old key");
        assert_eq!(fs::read(&path).unwrap(), b"old key", "the original moved");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&kept).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            keep_replaced(&dir.path().join("none"), "abc").unwrap(),
            None
        );
        for tag in ["", "../x", "a/b"] {
            assert!(keep_replaced(&path, tag).is_err(), "{tag:?}");
        }
    }

    /// A regular file or none can be replaced; a directory or a symlink
    /// cannot.
    #[test]
    fn only_a_regular_file_is_replaceable() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        fs::write(&file, "x").unwrap();
        check_replaceable(&file).unwrap();
        check_replaceable(&tmp.path().join("missing")).unwrap();
        assert!(check_replaceable(tmp.path()).is_err());
        #[cfg(unix)]
        {
            let link = tmp.path().join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(check_replaceable(&link).is_err());
        }
    }

    /// A file bind-mounted over another -- a Docker secret where the server
    /// keeps its own file -- cannot be replaced. Needs a mount namespace
    /// (`unshare -rm cargo test ...`); skipped elsewhere.
    #[cfg(unix)]
    #[test]
    fn a_file_mounted_in_place_is_not_replaceable() {
        let tmp = tempfile::tempdir().unwrap();
        let secret = tmp.path().join("secret");
        let target = tmp.path().join("target");
        fs::write(&secret, "mounted").unwrap();
        fs::write(&target, "own").unwrap();
        let mounted = std::process::Command::new("mount")
            .arg("--bind")
            .arg(&secret)
            .arg(&target)
            .stderr(std::process::Stdio::null())
            .status();
        if !mounted.is_ok_and(|status| status.success()) {
            eprintln!("skipped: cannot bind-mount here");
            return;
        }
        let refused = check_replaceable(&target);
        let _ = std::process::Command::new("umount").arg(&target).status();
        let error = refused.unwrap_err().to_string();
        assert!(error.contains("is mounted where it is"), "{error}");
    }

    #[test]
    fn mount_points_are_read_with_their_escapes_undone() {
        let table = "36 35 98:0 /mnt1 /mnt/with\\040space rw,noatime master:1 - ext3 /dev/root rw\n\
                     37 35 98:0 /secret /run/ca/signing-key.asc ro - ext4 /dev/sda1 rw\n";
        let points: Vec<PathBuf> = mount_points(table).collect();
        assert_eq!(
            points,
            [
                PathBuf::from("/mnt/with space"),
                PathBuf::from("/run/ca/signing-key.asc")
            ]
        );
    }
}
