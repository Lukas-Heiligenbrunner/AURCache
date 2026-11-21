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
/// The read walk grants what this lists, and a Landlock rule is attached to
/// what its path resolves to: granting `/var/run`, which leads to `/run`,
/// would grant all of `/run`, a protected path under it included. Leaving a
/// link out costs nothing: what it leads to, when that is clear of every
/// exclusion, is granted under its real name, and following a link needs no
/// right of its own. An entry whose type cannot be read is left out too.
fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_symlink()))
        .map(|entry| entry.path())
        .collect()
}

/// Expand exclusions into the positive read grants Landlock needs.
///
/// Walks from the filesystem root towards each excluded path, granting every
/// entry that is neither an excluded path nor an ancestor of one, and
/// descending into the ancestors. The excluded paths themselves are never
/// granted, so nothing beneath them is readable.
///
/// `list_dir` lists no symlink ([`list_dir`]), so none is ever granted or
/// descended. One on the way to an exclusion by name lists entries that lie
/// elsewhere: with `state -> srv`, `state/db` is `srv/db`, protected or not
/// by that name. Each exclusion is expected where it really is instead, and
/// the walk reaches it under its own name.
///
/// Not covered:
/// - A bind mount is no symlink: one showing an excluded directory elsewhere
///   grants it there. Only a deployment makes those, never a build.
/// - A hard link to an excluded file, in a granted directory, likewise: a
///   build or a parse cannot make one.
/// - A directory with case-insensitive lookup (casefold), where an exclusion
///   is spelled in another case than the stored name: the walk lists the
///   stored name, which matches no exclusion, and grants it. Comparing
///   `(st_dev, st_ino)` instead of names would close that.
/// - An entry swapped for a symlink after the walk looks at it: every grant
///   is opened by path once the walk is done, so the window is the whole
///   walk, and the grant follows the link. Nothing reaches that window in
///   the shipped layout, where every ancestor of a protected path is owned
///   by root or aurcache; it matters for a protected path under a directory
///   a build can write. Granting the very descriptor the walk looked at
///   (`PathBeneath::new` takes any `AsFd`) would close it.
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

    /// A fresh directory for a test that needs a real filesystem, resolved,
    /// so the paths it lists compare equal to the ones built from it.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    /// The real listing leaves every symlink out, dangling or not.
    #[test]
    fn lists_no_symlink() {
        let dir = scratch("aurcache-sandbox-list");
        std::fs::create_dir(dir.join("real")).unwrap();
        std::fs::write(dir.join("file"), "").unwrap();
        std::os::unix::fs::symlink("real", dir.join("alias")).unwrap();
        std::os::unix::fs::symlink("missing", dir.join("dangling")).unwrap();

        let mut listed = list_dir(&dir);
        listed.sort();
        assert_eq!(listed, vec![dir.join("file"), dir.join("real")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The walk over a real tree with links on the way: `var/run -> ../run`
    /// would grant `run` whole, `state -> srv` lists `state/db`, which is the
    /// protected `srv/db`, and neither is granted; what they lead to is,
    /// under its own name, but for the exclusions.
    #[test]
    fn grants_no_symlink_on_the_way_to_an_exclusion() {
        let dir = scratch("aurcache-sandbox-walk");
        for sub in ["run/secrets", "run/user", "var/lib/worker", "srv/db"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("srv/pub"), "").unwrap();
        std::os::unix::fs::symlink("../run", dir.join("var/run")).unwrap();
        std::os::unix::fs::symlink("srv", dir.join("state")).unwrap();
        // The real tree above `dir`, cut down to the way to it.
        let listing = |path: &Path| match dir.strip_prefix(path) {
            Ok(rest) => match rest.components().next() {
                Some(next) => vec![path.join(next)],
                None => list_dir(path),
            },
            Err(_) => list_dir(path),
        };

        let grants = read_grants_excluding(
            &[
                dir.join("run/secrets"),
                dir.join("var/lib/worker"),
                dir.join("srv/db"),
                dir.join("state/build_logs"),
            ],
            listing,
        );
        assert_eq!(grants, vec![dir.join("run/user"), dir.join("srv/pub")]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
