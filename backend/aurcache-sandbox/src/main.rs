//! Confine code from packages under one of three fixed profiles:
//!
//! ```text
//! aurcache-sandbox --allow-build-env -- PROGRAM [ARGS]...
//! aurcache-sandbox parse --pkgbuild FILE [--private PATH]... [--network] -- PROGRAM [ARGS]...
//! aurcache-sandbox offline -- PROGRAM [ARGS]...
//! ```
//!
//! - the build profile (`build.rs`): a worker's source download, which runs a
//!   PKGBUILD outside the chroot, may write only that build's own directories;
//! - `parse` (`parse.rs`): the server's PKGBUILD parsing, which may write
//!   nothing and read nothing private;
//! - `offline` (`offline.rs`): a build's dependency install, which runs other
//!   packages' scriptlets as root inside the chroot, reaches no network.
//!
//! Each profile owns its whole policy, stated by purpose rather than by
//! mechanism, so a caller cannot assemble a weaker one. What they share is
//! how a policy becomes Landlock rules, below.

use landlock::{AccessFs, BitFlags, PathBeneath, PathFd, RulesetCreated, RulesetCreatedAttr};
use std::path::{Path, PathBuf};

mod build;
mod offline;
mod parse;
mod seccomp;

fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.next_if(|arg| arg == "parse").is_some() {
        return parse::main(args);
    }
    if args.next_if(|arg| arg == "offline").is_some() {
        return offline::main(args);
    }
    build::main(args)
}

/// Add one path to the ruleset, narrowing the rights to what its file type can
/// actually carry.
///
/// Directory-only rights (`ReadDir`, `MakeDir`, `RemoveFile`, …) cannot apply
/// to a regular file. Requesting them anyway makes the ruleset only
/// *partially* enforced, which this binary treats as failure — so a single
/// regular file among the granted paths would otherwise refuse every build.
/// `/` contains exactly that on a normal system.
///
/// A path that does not exist is skipped, not an error: `BUILDDIR` in
/// particular may be created by makepkg itself. Skipping leaves it unwritable,
/// which surfaces as a build failure rather than as a silently widened policy.
fn grant(
    ruleset: RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
) -> Result<RulesetCreated, Box<dyn std::error::Error>> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Ok(ruleset);
    };
    let access = if metadata.is_dir() {
        access
    } else {
        access & (AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Execute)
    };
    if access.is_empty() {
        return Ok(ruleset);
    }
    let Ok(fd) = PathFd::new(path) else {
        return Ok(ruleset);
    };
    Ok(ruleset.add_rule(PathBeneath::new(fd, access))?)
}

/// The entries of `dir` that are not symlinks; none when it cannot be read.
///
/// A symlink is left out because granting it would grant its target:
/// [`grant`] opens the path, which follows the link, and the rule then covers
/// everything beneath wherever it points. A Kubernetes secret is
/// `key -> ..data/key` with `..data -> ..<timestamp>`, so granting `..data`
/// granted the very secret kept private as `key`; `/var/run -> /run` did the
/// same for systemd credentials. Leaving links out costs nothing: a link's
/// target is granted at its own path, where the walk reaches it -- unless it
/// is private, which is the point. Landlock checks the resolved file, so
/// reading through a link still works wherever its target is granted.
fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| !e.file_type().is_ok_and(|t| t.is_symlink()))
        .map(|e| e.path())
        .collect()
}

/// Expand exclusions into the positive read grants Landlock needs.
///
/// Walks from the filesystem root towards each excluded path, granting every
/// entry that is neither an excluded path nor an ancestor of one, and
/// descending into the ancestors. The excluded paths themselves are never
/// granted, so nothing beneath them is readable.
fn read_grants_excluding(
    excludes: &[PathBuf],
    list_dir: impl Fn(&Path) -> Vec<PathBuf> + Copy,
) -> Vec<PathBuf> {
    fn walk(
        dir: &Path,
        excludes: &[PathBuf],
        list_dir: impl Fn(&Path) -> Vec<PathBuf> + Copy,
        out: &mut Vec<PathBuf>,
    ) {
        for entry in list_dir(dir) {
            if excludes.contains(&entry) {
                // The exclusion itself: grant nothing, so its whole subtree
                // stays unreadable.
                continue;
            }
            if excludes.iter().any(|e| e.starts_with(&entry)) {
                // On the path to an exclusion: descend rather than granting the
                // whole subtree, which would grant the exclusion with it.
                walk(&entry, excludes, list_dir, out);
                continue;
            }
            out.push(entry);
        }
    }

    let mut out = Vec::new();
    walk(Path::new("/"), excludes, list_dir, &mut out);
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test, removed when dropped. By hand: the
    /// sandbox has no dependencies, and a test helper is no reason to add one.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("aurcache-sandbox-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Whether any grant reaches `secret`, through any link.
    fn reaches(grants: &[PathBuf], secret: &Path) -> Option<PathBuf> {
        let secret = std::fs::canonicalize(secret).unwrap();
        grants
            .iter()
            .find(|g| std::fs::canonicalize(g).is_ok_and(|real| secret.starts_with(real)))
            .cloned()
    }

    /// Both the path as configured and its real path, as the server names
    /// private paths.
    fn private(path: &Path) -> Vec<PathBuf> {
        vec![path.to_path_buf(), std::fs::canonicalize(path).unwrap()]
    }

    /// A Kubernetes secret volume is two links deep: `key -> ..data/key` and
    /// `..data -> ..<timestamp>`. Granting `..data` grants the secret.
    #[test]
    fn a_kubernetes_secret_is_not_granted_through_its_links() {
        let root = Scratch::new("k8s-secret");
        let volume = root.path().join("secret");
        let stamped = volume.join("..2026_11_17_10_00_00.123");
        std::fs::create_dir_all(&stamped).unwrap();
        std::fs::write(stamped.join("key"), "secret").unwrap();
        std::fs::write(stamped.join("other"), "not secret").unwrap();
        std::os::unix::fs::symlink("..2026_11_17_10_00_00.123", volume.join("..data")).unwrap();
        std::os::unix::fs::symlink("..data/key", volume.join("key")).unwrap();
        std::os::unix::fs::symlink("..data/other", volume.join("other")).unwrap();

        let grants = read_grants_excluding(&private(&volume.join("key")), list_dir);
        assert_eq!(reaches(&grants, &stamped.join("key")), None);
        // What is beside it stays readable, at its real path.
        assert!(reaches(&grants, &stamped.join("other")).is_some());
    }

    /// `/var/run -> /run`: a credential under `/run` named through `/var/run`
    /// is not granted through the link.
    #[test]
    fn a_credential_under_a_linked_directory_is_not_granted_through_it() {
        let root = Scratch::new("var-run");
        let credentials = root.path().join("run/credentials/aurcache.service");
        std::fs::create_dir_all(&credentials).unwrap();
        std::fs::write(credentials.join("ssh_key"), "secret").unwrap();
        std::fs::create_dir_all(root.path().join("var")).unwrap();
        std::os::unix::fs::symlink("../run", root.path().join("var/run")).unwrap();

        let configured = root
            .path()
            .join("var/run/credentials/aurcache.service/ssh_key");
        let grants = read_grants_excluding(&private(&configured), list_dir);
        assert_eq!(reaches(&grants, &credentials.join("ssh_key")), None);
        assert!(!grants.contains(&root.path().join("var/run")));
    }

    /// The whole point of the walk: a short list of secrets stays unreadable
    /// while everything else is granted, without enumerating the filesystem by
    /// hand.
    #[test]
    fn excludes_a_top_level_directory_and_grants_the_rest() {
        let listing = |dir: &Path| -> Vec<PathBuf> {
            match dir.to_str().unwrap() {
                "/" => ["/app", "/usr", "/etc", "/var"]
                    .iter()
                    .map(PathBuf::from)
                    .collect(),
                _ => Vec::new(),
            }
        };
        let grants = read_grants_excluding(&[PathBuf::from("/app")], listing);
        assert_eq!(
            grants,
            vec![
                PathBuf::from("/etc"),
                PathBuf::from("/usr"),
                PathBuf::from("/var")
            ]
        );
        assert!(!grants.contains(&PathBuf::from("/app")));
    }

    /// A nested exclusion must not drag its parents down with it: /var stays
    /// readable except for the one directory being protected.
    #[test]
    fn descends_to_a_nested_exclusion_granting_siblings_on_the_way() {
        let listing = |dir: &Path| -> Vec<PathBuf> {
            match dir.to_str().unwrap() {
                "/" => vec![PathBuf::from("/usr"), PathBuf::from("/var")],
                "/var" => vec![PathBuf::from("/var/lib"), PathBuf::from("/var/log")],
                "/var/lib" => vec![
                    PathBuf::from("/var/lib/aurcache-worker"),
                    PathBuf::from("/var/lib/pacman"),
                ],
                _ => Vec::new(),
            }
        };
        let grants = read_grants_excluding(&[PathBuf::from("/var/lib/aurcache-worker")], listing);
        assert_eq!(
            grants,
            vec![
                PathBuf::from("/usr"),
                PathBuf::from("/var/lib/pacman"),
                PathBuf::from("/var/log"),
            ]
        );
        // Neither the secret nor a blanket grant of its parents.
        assert!(!grants.contains(&PathBuf::from("/var")));
        assert!(!grants.contains(&PathBuf::from("/var/lib")));
        assert!(!grants.contains(&PathBuf::from("/var/lib/aurcache-worker")));
    }

    #[test]
    fn handles_several_exclusions_at_once() {
        let listing = |dir: &Path| -> Vec<PathBuf> {
            match dir.to_str().unwrap() {
                "/" => ["/app", "/srv", "/usr"].iter().map(PathBuf::from).collect(),
                _ => Vec::new(),
            }
        };
        let grants =
            read_grants_excluding(&[PathBuf::from("/app"), PathBuf::from("/srv")], listing);
        assert_eq!(grants, vec![PathBuf::from("/usr")]);
    }
}
