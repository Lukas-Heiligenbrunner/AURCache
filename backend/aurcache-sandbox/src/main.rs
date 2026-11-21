//! Confine code from packages under one of three fixed profiles:
//!
//! ```text
//! aurcache-sandbox --allow-build-env -- PROGRAM [ARGS]...
//! aurcache-sandbox parse --pkgbuild FILE [--private PATH]... [--network] -- PROGRAM [ARGS]...
//! aurcache-sandbox offline -- PROGRAM [ARGS]...
//! aurcache-sandbox --capabilities --agent-dir DIR
//! ```
//!
//! - the build profile (`build.rs`): a worker's source download, which runs a
//!   PKGBUILD outside the chroot, may write only that build's own directories;
//! - `parse` (`parse.rs`): the server's PKGBUILD parsing, which may write
//!   nothing and read nothing private;
//! - `offline` (`offline.rs`): a build's dependency install, which runs other
//!   packages' scriptlets as root inside the chroot, reaches no network;
//! - `--capabilities` runs nothing: it says what the build profile would
//!   enforce beyond what every version has, if a build started now, as the
//!   user it runs as -- for a caller that relies on it to ask first.
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
    if args.next_if(|arg| arg == CAPABILITIES_ARG).is_some() {
        return build::capabilities(args);
    }
    build::main(args)
}

// What a caller has to agree with this program on beyond its profiles. The
// worker keeps copies of these, which its tests hold to these definitions.

/// The argument that makes the sandbox say what the build profile would
/// enforce, and run nothing: `--capabilities --agent-dir DIR`. A sandbox that
/// predates it refuses it as a usage error.
const CAPABILITIES_ARG: &str = "--capabilities";

/// The argument naming the directory the caller's ssh-agents' sockets are
/// beneath.
///
/// Given by the caller rather than assumed: the worker's data directory is
/// its own configuration, which this program -- reached through `sudo`, with
/// the worker's environment gone -- never sees, so a default would answer for
/// a directory the worker may not use.
const AGENT_DIR_ARG: &str = "--agent-dir";

/// The line `--capabilities` prints when a build started now would be refused
/// every pathname UNIX socket beneath the agents' directory but the one it is
/// offered.
const RESOLVE_UNIX: &str = "resolve-unix";

/// The line it prints instead, followed by `: ` and why not.
const NO_RESOLVE_UNIX: &str = "no-resolve-unix";

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
    // `ResolveUnix` is a file right too: it is how one socket is granted.
    let access = if metadata.is_dir() {
        access
    } else {
        access
            & (AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Execute | AccessFs::ResolveUnix)
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

/// Where a directory entry leads, as the read walk sees it.
enum Leads {
    /// It is not a symlink.
    Itself,
    /// A symlink, to this real path.
    To(PathBuf),
    /// A symlink that cannot be resolved, or an entry that cannot be looked at.
    Nowhere,
}

/// Where `path` leads.
fn leads(path: &Path) -> Leads {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() => Leads::Itself,
        Ok(_) => std::fs::canonicalize(path).map_or(Leads::Nowhere, Leads::To),
        Err(_) => Leads::Nowhere,
    }
}

/// `paths`, each also where it really is when that differs.
///
/// The read walk keeps a path unreadable as it is written, so a secret behind
/// a symlink -- a Kubernetes secret is `key -> ..data/key` -- or under a linked
/// directory would otherwise stay readable where it really is. A path that does
/// not exist yet is placed by its parent, so one created later under a linked
/// directory is covered too.
fn with_real_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut real: Vec<PathBuf> = paths
        .iter()
        .filter_map(|path| real_path(path))
        .filter(|real| !paths.contains(real))
        .collect();
    real.sort();
    real.dedup();
    paths.into_iter().chain(real).collect()
}

/// Where `path` really is: resolved if it exists, else its resolved parent
/// joined with its name. `None` when neither can be resolved.
fn real_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok().or_else(|| {
        let name = path.file_name()?;
        Some(std::fs::canonicalize(path.parent()?).ok()?.join(name))
    })
}

/// Expand exclusions into the positive read grants Landlock needs.
///
/// Walks from the filesystem root towards each excluded path, granting every
/// entry that is neither an excluded path nor an ancestor of one, and
/// descending into the ancestors. The excluded paths themselves are never
/// granted, so nothing beneath them is readable.
///
/// A rule is attached to what a path resolves to, so a symlink is granted only
/// when it leads clear of every exclusion: `/var/run` leads to `/run`, and
/// granting it would grant all of `/run`, a protected path under it included.
/// One that cannot be resolved is not granted at all.
fn read_grants_excluding(
    excludes: &[PathBuf],
    list_dir: impl Fn(&Path) -> Vec<PathBuf> + Copy,
    leads: impl Fn(&Path) -> Leads + Copy,
) -> Vec<PathBuf> {
    fn walk(
        dir: &Path,
        excludes: &[PathBuf],
        list_dir: impl Fn(&Path) -> Vec<PathBuf> + Copy,
        leads: impl Fn(&Path) -> Leads + Copy,
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
                walk(&entry, excludes, list_dir, leads, out);
                continue;
            }
            let clear = match leads(&entry) {
                Leads::Itself => true,
                Leads::To(real) => !excludes
                    .iter()
                    .any(|e| e.starts_with(&real) || real.starts_with(e)),
                Leads::Nowhere => false,
            };
            if clear {
                out.push(entry);
            }
        }
    }

    let mut out = Vec::new();
    walk(Path::new("/"), excludes, list_dir, leads, &mut out);
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_links(_: &Path) -> Leads {
        Leads::Itself
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
        let grants = read_grants_excluding(&[PathBuf::from("/app")], listing, no_links);
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
        let grants = read_grants_excluding(
            &[PathBuf::from("/var/lib/aurcache-worker")],
            listing,
            no_links,
        );
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
        let grants = read_grants_excluding(
            &[PathBuf::from("/app"), PathBuf::from("/srv")],
            listing,
            no_links,
        );
        assert_eq!(grants, vec![PathBuf::from("/usr")]);
    }

    /// A symlink is granted by what it leads to: `/var/run -> /run` would
    /// grant all of `/run`, and a secret under it, while `/run` itself is
    /// walked for its siblings.
    #[test]
    fn grants_no_symlink_that_leads_to_an_exclusion_or_its_ancestor() {
        let listing = |dir: &Path| -> Vec<PathBuf> {
            let entries: &[&str] = match dir.to_str().unwrap() {
                "/" => &["/run", "/usr", "/var"],
                "/run" => &["/run/secrets", "/run/user"],
                "/var" => &[
                    "/var/run",
                    "/var/into",
                    "/var/elsewhere",
                    "/var/dangling",
                    "/var/lib",
                ],
                _ => &[],
            };
            entries.iter().map(PathBuf::from).collect()
        };
        let leads = |path: &Path| match path.to_str().unwrap() {
            "/var/run" => Leads::To(PathBuf::from("/run")),
            "/var/into" => Leads::To(PathBuf::from("/run/secrets/key")),
            "/var/elsewhere" => Leads::To(PathBuf::from("/usr/share")),
            "/var/dangling" => Leads::Nowhere,
            _ => Leads::Itself,
        };
        let grants = read_grants_excluding(
            &[
                PathBuf::from("/run/secrets"),
                PathBuf::from("/var/lib/aurcache-worker"),
            ],
            listing,
            leads,
        );
        assert_eq!(
            grants,
            vec![
                PathBuf::from("/run/user"),
                PathBuf::from("/usr"),
                PathBuf::from("/var/elsewhere"),
            ]
        );
    }
}
