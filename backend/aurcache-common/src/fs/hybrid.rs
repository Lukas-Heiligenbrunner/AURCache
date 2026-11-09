//! Keeping the CA and the build logs in the repository until there is a
//! better place for them.
//!
//! The hybrid image is what a 0.5 deployment upgrades to by changing its image
//! line, and the only directory such a deployment persists is the repository.
//! So when the server's own directories for the worker CA and the build logs
//! are not on a volume, and were not configured, it keeps them inside the
//! repository, as `repo/.ca` and `repo/.build_logs`. Once its own are on a
//! volume -- `/app` mounted, on this image or on the split one -- it finds them
//! there at boot and moves them across, rather than starting with a CA no
//! enrolled worker trusts and no logs. Both images behave alike, so moving
//! from one to the other changes nothing but the embedded worker.
//!
//! A move is resumable at every step. Within one filesystem it is a single
//! rename. Across two, the tree is copied into `.adopting` inside the target,
//! which is renamed to `.adopted` only once the copy is whole; the original is
//! removed only after that, and the entries leave `.adopted` last. A boot that
//! finds `.adopting` copies again from the intact original, and one that finds
//! `.adopted` finishes the move.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::REPO_ROOT;

/// A copy under way: incomplete, and discarded if a boot finds it.
const COPYING: &str = ".adopting";
/// A whole copy, or the original renamed in: the move is only finishing.
const COPIED: &str = ".adopted";

/// One of the directories the hybrid image keeps inside the repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HybridDir {
    Ca,
    BuildLogs,
}

impl HybridDir {
    pub const ALL: [Self; 2] = [Self::Ca, Self::BuildLogs];

    /// Where the hybrid image keeps it.
    #[must_use]
    pub fn hybrid_path(self) -> PathBuf {
        Path::new(REPO_ROOT).join(match self {
            Self::Ca => ".ca",
            Self::BuildLogs => ".build_logs",
        })
    }

    /// Where this server is configured to keep it.
    fn configured_path(self) -> PathBuf {
        match self {
            Self::Ca => super::configured_ca_dir(),
            Self::BuildLogs => super::configured_build_log_root(),
        }
    }

    /// Whether the operator chose that directory, rather than the default.
    fn chosen(self) -> bool {
        let var = match self {
            Self::Ca => super::CA_DIR_VAR,
            Self::BuildLogs => super::BUILD_LOG_PATH_VAR,
        };
        std::env::var_os(var).is_some()
    }

    /// What it is, for a log line.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Ca => "the worker CA",
            Self::BuildLogs => "the build logs",
        }
    }

    /// Where it is when it is not moved: see [`Adoption::InRepository`].
    pub(super) fn in_repository(self) -> &'static OnceLock<PathBuf> {
        static CA: OnceLock<PathBuf> = OnceLock::new();
        static BUILD_LOGS: OnceLock<PathBuf> = OnceLock::new();
        match self {
            Self::Ca => &CA,
            Self::BuildLogs => &BUILD_LOGS,
        }
    }
}

/// What became of one hybrid directory at boot.
#[derive(Debug)]
pub struct Adopted {
    pub dir: HybridDir,
    pub outcome: Adoption,
}

/// What to do with a configured directory that is not persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unpersisted {
    /// Use the repository's instead: the default, when the repository is on
    /// a volume.
    UseRepository,
    /// Use it, unless that would strand what the repository already holds:
    /// a directory the operator chose, or nowhere better to go.
    UseAsGiven,
}

/// What became of a hybrid directory, and where it went.
#[derive(Debug, PartialEq, Eq)]
pub enum Adoption {
    /// The configured directory is used, and there was nothing to move.
    Untouched,
    /// Moved into the configured directory.
    Moved { to: PathBuf },
    /// Used from the repository, because the configured directory is not
    /// persisted: anything there would be lost with the container.
    InRepository { configured: PathBuf },
    /// The configured directory has contents of its own. It is used, and the
    /// hybrid one is left for someone to look at.
    Conflict { configured: PathBuf },
}

/// Move whatever the hybrid image left in the repository to where this server
/// keeps it, for every [`HybridDir`].
///
/// `persisted` says whether a directory outlives the container it is in. Run
/// it before anything reads [`super::ca_dir`] or [`super::build_log_root`]:
/// those answer with the hybrid path from here on when one is kept in place.
///
/// # Errors
///
/// When a move cannot go on. The server must not start then: it would create
/// a CA of its own, which no enrolled worker trusts.
pub fn adopt_hybrid_dirs(persisted: impl Fn(&Path) -> bool) -> io::Result<Vec<Adopted>> {
    let repo_persisted = persisted(Path::new(REPO_ROOT));
    let mut adopted = Vec::new();
    for dir in HybridDir::ALL {
        let hybrid = dir.hybrid_path();
        let unpersisted = if repo_persisted && !dir.chosen() {
            Unpersisted::UseRepository
        } else {
            Unpersisted::UseAsGiven
        };
        let outcome = adopt(&hybrid, &dir.configured_path(), unpersisted, &persisted)?;
        if matches!(outcome, Adoption::InRepository { .. }) {
            create_private(&hybrid)?;
            let _ = dir.in_repository().set(hybrid);
        }
        adopted.push(Adopted { dir, outcome });
    }
    Ok(adopted)
}

/// Move `hybrid` into `configured`, or say why not.
fn adopt(
    hybrid: &Path,
    configured: &Path,
    unpersisted: Unpersisted,
    persisted: impl Fn(&Path) -> bool,
) -> io::Result<Adoption> {
    let copied = configured.join(COPIED);
    if copied.exists() {
        finish(hybrid, configured)?;
        return Ok(Adoption::Moved {
            to: configured.to_path_buf(),
        });
    }
    if same_dir(hybrid, configured) {
        return Ok(Adoption::Untouched);
    }
    let held = has_entries(hybrid)?;
    if !persisted(configured) && (held || unpersisted == Unpersisted::UseRepository) {
        return Ok(Adoption::InRepository {
            configured: configured.to_path_buf(),
        });
    }
    if !held {
        return Ok(Adoption::Untouched);
    }
    if has_entries(configured)? {
        return Ok(Adoption::Conflict {
            configured: configured.to_path_buf(),
        });
    }

    if !configured.exists() {
        // Closed as the original was: nothing else may have made it yet.
        fs::create_dir_all(configured)?;
        fs::set_permissions(configured, fs::metadata(hybrid)?.permissions())?;
    }
    let copying = configured.join(COPYING);
    if copying.exists() {
        fs::remove_dir_all(&copying)?;
    }
    if fs::rename(hybrid, &copied).is_err() {
        // Another filesystem, or the hybrid directory is a mount of its own.
        copy_tree(hybrid, &copying)?;
        fs::rename(&copying, &copied)?;
        sync_dir(configured)?;
    }
    finish(hybrid, configured)?;
    Ok(Adoption::Moved {
        to: configured.to_path_buf(),
    })
}

/// Complete a move whose copy is whole: drop the original, then bring the
/// entries out of [`COPIED`].
fn finish(hybrid: &Path, configured: &Path) -> io::Result<()> {
    if hybrid.exists() {
        empty(hybrid)?;
        // A mount point cannot go; its contents are what mattered.
        let _ = fs::remove_dir(hybrid);
    }
    let copied = configured.join(COPIED);
    for entry in fs::read_dir(&copied)? {
        let entry = entry?;
        fs::rename(entry.path(), configured.join(entry.file_name()))?;
    }
    fs::remove_dir(&copied)?;
    sync_dir(configured)
}

/// Make sure a directory exists, closed to other users: see
/// `docker/private-state.sh`, which does the same for the configured ones.
fn create_private(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Whether a directory exists and holds anything but an unfinished copy.
fn has_entries(dir: &Path) -> io::Result<bool> {
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                if entry?.file_name() != COPYING {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Remove everything inside a directory, leaving the directory.
fn empty(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if fs::symlink_metadata(&path)?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Copy a tree with its permissions, each file synced. Iterative, like
/// [`super::measure_directory`]: nobody controls how deep a log tree is.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        fs::create_dir(&to)?;
        fs::set_permissions(&to, fs::metadata(&from)?.permissions())?;
        for entry in fs::read_dir(&from)? {
            let entry = entry?;
            let (source, target) = (entry.path(), to.join(entry.file_name()));
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push((source, target));
            } else if kind.is_file() {
                fs::copy(&source, &target)?;
                fs::File::open(&target)?.sync_all()?;
            } else if kind.is_symlink() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(fs::read_link(&source)?, &target)?;
            }
        }
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Layout {
        _root: tempfile::TempDir,
        hybrid: PathBuf,
        configured: PathBuf,
    }

    /// A hybrid `.ca` holding a key and a nested directory, and an empty
    /// configured directory, as the image's entrypoint leaves it.
    fn layout() -> Layout {
        let root = tempfile::tempdir().unwrap();
        let hybrid = root.path().join("repo/.ca");
        let configured = root.path().join("data/ca");
        fs::create_dir_all(hybrid.join("issued")).unwrap();
        fs::write(hybrid.join("ca.key"), "key").unwrap();
        fs::write(hybrid.join("issued/w1.pem"), "cert").unwrap();
        fs::create_dir_all(&configured).unwrap();
        Layout {
            _root: root,
            hybrid,
            configured,
        }
    }

    fn assert_moved(l: &Layout) {
        assert_eq!(
            fs::read_to_string(l.configured.join("ca.key")).unwrap(),
            "key"
        );
        assert_eq!(
            fs::read_to_string(l.configured.join("issued/w1.pem")).unwrap(),
            "cert"
        );
        assert!(!l.hybrid.exists());
        assert!(!l.configured.join(COPIED).exists());
        assert!(!l.configured.join(COPYING).exists());
    }

    /// A native install makes no build log directory before the server does.
    #[cfg(unix)]
    #[test]
    fn a_configured_directory_made_for_the_move_is_closed_like_the_original() {
        use std::os::unix::fs::PermissionsExt;
        let l = layout();
        fs::remove_dir(&l.configured).unwrap();
        fs::set_permissions(&l.hybrid, fs::Permissions::from_mode(0o700)).unwrap();
        adopt(&l.hybrid, &l.configured, Unpersisted::UseAsGiven, |_| true).unwrap();
        let mode = fs::metadata(&l.configured).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert_moved(&l);
    }

    #[test]
    fn an_empty_configured_directory_takes_the_hybrid_one() {
        let l = layout();
        let outcome = adopt(&l.hybrid, &l.configured, Unpersisted::UseRepository, |_| {
            true
        })
        .unwrap();
        assert_eq!(
            outcome,
            Adoption::Moved {
                to: l.configured.clone()
            }
        );
        assert_moved(&l);
    }

    /// The path a separate volume takes, where the rename cannot.
    #[test]
    fn a_copy_keeps_the_tree_and_its_permissions() {
        let l = layout();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&l.hybrid, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(l.hybrid.join("ca.key"), fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let copying = l.configured.join(COPYING);
        copy_tree(&l.hybrid, &copying).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&copying), 0o700);
            assert_eq!(mode(&copying.join("ca.key")), 0o600);
        }
        fs::rename(&copying, l.configured.join(COPIED)).unwrap();
        finish(&l.hybrid, &l.configured).unwrap();
        assert_moved(&l);
    }

    /// Stopped mid-copy: the copy is thrown away and made again from the
    /// original, which nothing has touched yet.
    #[test]
    fn a_partial_copy_is_made_again() {
        let l = layout();
        fs::create_dir_all(l.configured.join(COPYING)).unwrap();
        fs::write(l.configured.join(COPYING).join("ca.key"), "ke").unwrap();
        adopt(&l.hybrid, &l.configured, Unpersisted::UseRepository, |_| {
            true
        })
        .unwrap();
        assert_moved(&l);
    }

    /// Stopped after the copy was whole, part-way through removing the
    /// original: the copy is what counts, and the move finishes.
    #[test]
    fn a_whole_copy_finishes_whatever_is_left_of_the_original() {
        let l = layout();
        copy_tree(&l.hybrid, &l.configured.join(COPIED)).unwrap();
        fs::remove_file(l.hybrid.join("ca.key")).unwrap();
        adopt(&l.hybrid, &l.configured, Unpersisted::UseRepository, |_| {
            true
        })
        .unwrap();
        assert_moved(&l);
    }

    /// Stopped part-way through bringing the entries out.
    #[test]
    fn entries_already_out_stay_out() {
        let l = layout();
        fs::rename(&l.hybrid, l.configured.join(COPIED)).unwrap();
        fs::rename(
            l.configured.join(COPIED).join("ca.key"),
            l.configured.join("ca.key"),
        )
        .unwrap();
        adopt(&l.hybrid, &l.configured, Unpersisted::UseRepository, |_| {
            true
        })
        .unwrap();
        assert_moved(&l);
    }

    /// Moving onto the container's own layer would lose it at the next
    /// update; where it is, it survives.
    #[test]
    fn an_unpersisted_configured_directory_leaves_it_in_place() {
        for how in [Unpersisted::UseRepository, Unpersisted::UseAsGiven] {
            let l = layout();
            let outcome = adopt(&l.hybrid, &l.configured, how, |_| false).unwrap();
            assert_eq!(
                outcome,
                Adoption::InRepository {
                    configured: l.configured.clone()
                }
            );
            assert!(l.hybrid.join("ca.key").exists());
        }
    }

    /// A 0.5 deployment's first start on this image: nothing yet, and only
    /// the repository on a volume.
    #[test]
    fn an_unpersisted_default_starts_in_the_repository() {
        let l = layout();
        fs::remove_dir_all(&l.hybrid).unwrap();
        let unless_configured = |p: &Path| p != l.configured;
        let outcome = adopt(
            &l.hybrid,
            &l.configured,
            Unpersisted::UseRepository,
            unless_configured,
        );
        assert_eq!(
            outcome.unwrap(),
            Adoption::InRepository {
                configured: l.configured.clone()
            }
        );
    }

    /// Set by hand, a directory is used as given while nothing would be lost.
    #[test]
    fn an_explicit_directory_is_used_as_given() {
        let l = layout();
        fs::remove_dir_all(&l.hybrid).unwrap();
        let outcome = adopt(&l.hybrid, &l.configured, Unpersisted::UseAsGiven, |_| false);
        assert_eq!(outcome.unwrap(), Adoption::Untouched);
    }

    #[test]
    fn a_configured_directory_with_contents_is_not_overwritten() {
        let l = layout();
        fs::write(l.configured.join("ca.key"), "other").unwrap();
        let outcome = adopt(&l.hybrid, &l.configured, Unpersisted::UseRepository, |_| {
            true
        })
        .unwrap();
        assert_eq!(
            outcome,
            Adoption::Conflict {
                configured: l.configured.clone()
            }
        );
        assert_eq!(
            fs::read_to_string(l.configured.join("ca.key")).unwrap(),
            "other"
        );
        assert!(l.hybrid.join("ca.key").exists());
    }

    /// The hybrid image itself is configured with these very directories.
    #[test]
    fn the_hybrid_image_keeps_its_own() {
        let l = layout();
        let outcome = adopt(&l.hybrid, &l.hybrid, Unpersisted::UseRepository, |_| true).unwrap();
        assert_eq!(outcome, Adoption::Untouched);
        assert!(l.hybrid.join("ca.key").exists());
    }

    #[test]
    fn nothing_to_adopt_is_nothing_done() {
        let l = layout();
        fs::remove_dir_all(&l.hybrid).unwrap();
        let outcome = adopt(&l.hybrid, &l.configured, Unpersisted::UseRepository, |_| {
            true
        })
        .unwrap();
        assert_eq!(outcome, Adoption::Untouched);
    }
}
