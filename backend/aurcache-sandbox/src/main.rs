//! Confine PKGBUILD code with Landlock, under one of two fixed profiles:
//!
//! ```text
//! aurcache-sandbox --allow-build-env -- PROGRAM [ARGS]...
//! aurcache-sandbox parse --pkgbuild FILE [--private PATH]... [--network] -- PROGRAM [ARGS]...
//! ```
//!
//! - the build profile (`build.rs`): a worker's source download, which runs a
//!   PKGBUILD outside the chroot, may write only that build's own directories;
//! - `parse` (`parse.rs`): the server's PKGBUILD parsing, which may write
//!   nothing and read nothing private.
//!
//! Each profile owns its whole policy, stated by purpose rather than by
//! mechanism, so a caller cannot assemble a weaker one. What they share is
//! how a policy becomes Landlock rules, below.

use landlock::{AccessFs, BitFlags, PathBeneath, PathFd, RulesetCreated, RulesetCreatedAttr};
use std::path::{Path, PathBuf};

mod build;
mod parse;

fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.next_if(|arg| arg == "parse").is_some() {
        return parse::main(args);
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

/// The entries of `dir`; none when it cannot be read.
fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries.flatten().map(|e| e.path()).collect()
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
