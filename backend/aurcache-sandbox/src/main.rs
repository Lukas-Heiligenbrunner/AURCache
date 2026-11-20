//! Confine code from packages under one of four fixed profiles:
//!
//! ```text
//! aurcache-sandbox --allow-build-env -- PROGRAM [ARGS]...
//! aurcache-sandbox parse --pkgbuild FILE [--private PATH]... [--network] -- PROGRAM [ARGS]...
//! aurcache-sandbox offline -- PROGRAM [ARGS]...
//! aurcache-sandbox container -- PROGRAM [ARGS]...
//! aurcache-sandbox --capabilities --agent-dir DIR
//! ```
//!
//! - the build profile (`build.rs`): a worker's source download, which runs a
//!   PKGBUILD outside the chroot, may write only that build's own directories,
//!   and opens no vsock socket;
//! - `parse` (`parse.rs`): the server's PKGBUILD parsing, which may write
//!   nothing, read nothing private, and open no vsock socket;
//! - `offline` (`offline.rs`): a build's dependency install, which runs other
//!   packages' scriptlets as root inside the chroot, reaches no network;
//! - `container` (`container.rs`): a build's containers, which run the
//!   PKGBUILD and its dependencies' code, open no vsock socket;
//! - `--capabilities` runs nothing: it says what the build profile would
//!   enforce beyond what every version has, if a build started now, as the
//!   user it runs as -- for a caller that relies on it to ask first.
//!
//! Each profile owns its whole policy, stated by purpose rather than by
//! mechanism, so a caller cannot assemble a weaker one. What they share is
//! how a policy becomes Landlock rules, below.

use landlock::{AccessFs, BitFlags, PathBeneath, PathFd, RulesetCreated, RulesetCreatedAttr};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

mod build;
mod container;
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
    if args.next_if(|arg| arg == "container").is_some() {
        return container::main(args);
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

/// `paths`, each where it really is ([`real_path`]); as written only when
/// that cannot be worked out.
///
/// The read walk reaches a path under its real name only, as it lists no
/// symlink, so a secret behind one -- a Kubernetes secret is
/// `key -> ..data/key` -- or under a linked directory would otherwise stay
/// readable where it really is. A path that does not exist yet is placed
/// where it will be, so one created later is covered too, behind a dangling
/// link as well.
fn with_real_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|path| real_path(&path).unwrap_or(path))
        .collect()
}

/// `listed`, each where it really is ([`with_real_paths`]), to be kept
/// unreadable by [`read_grants_excluding`]; or, as the error, the listed path
/// that really is `/`. The walk never meets `/` as an entry, so it would
/// ignore that one without a word while honouring the rest, and protecting `/`
/// cannot mean anything a confined program can run under either: it could read
/// nothing, its own executable included.
fn exclusions(listed: &[PathBuf]) -> Result<Vec<PathBuf>, PathBuf> {
    let real = with_real_paths(listed.to_vec());
    match real.iter().position(|path| path == Path::new("/")) {
        Some(at) => Err(listed[at].clone()),
        None => Ok(real),
    }
}

/// How many symlinks [`real_path`] follows before giving up, as many as the
/// kernel does.
const MAX_LINKS: usize = 40;

/// Where `path` really is, whether or not it exists, walked as the kernel
/// walks it: from `/`, or the current directory for a relative path, one
/// component after the other, every symlink on the way followed, dangling
/// ones included. A missing name is assumed to be created as a directory, as
/// it must be for anything to exist below it: the names after it are added
/// as written, and a `..` among them cancels the last, so `gone/..` is where
/// `gone` would be created. A symlink created there instead is not predicted.
/// A name whose lookup fails for another reason than not existing, such as
/// `EACCES` or `ENAMETOOLONG`, is taken as missing too. For `EACCES` that is
/// safe: the confined process has no more rights than this one, so it cannot
/// look up there either. `None` for a link loop or more than [`MAX_LINKS`]
/// links, or for a relative path when the current directory cannot be read.
fn real_path(path: &Path) -> Option<PathBuf> {
    // The components still to walk, the next one last. A link's target takes
    // its place, to be walked from the directory holding the link.
    let mut todo: Vec<PathBuf> = path
        .components()
        .rev()
        .map(|part| part.as_os_str().into())
        .collect();
    let mut real = if path.is_absolute() {
        PathBuf::from("/")
    } else {
        std::env::current_dir().ok()?
    };
    // The names below `real` that do not exist yet, the deepest last.
    let mut missing: Vec<OsString> = Vec::new();
    let mut links = 0;
    while let Some(part) = todo.pop() {
        match part.components().next() {
            Some(Component::RootDir) => real = PathBuf::from("/"),
            Some(Component::ParentDir) => {
                if missing.pop().is_none() {
                    // `/..` is `/`.
                    real.pop();
                }
            }
            Some(Component::Normal(name)) if missing.is_empty() => {
                let next = real.join(name);
                if let Ok(target) = std::fs::read_link(&next) {
                    links += 1;
                    if links > MAX_LINKS {
                        return None;
                    }
                    todo.extend(
                        target
                            .components()
                            .rev()
                            .map(|part| part.as_os_str().into()),
                    );
                } else if next.symlink_metadata().is_ok() {
                    real = next;
                } else {
                    missing.push(name.to_owned());
                }
            }
            Some(Component::Normal(name)) => missing.push(name.to_owned()),
            Some(Component::CurDir | Component::Prefix(_)) | None => {}
        }
    }
    Some(missing.into_iter().fold(real, |real, name| real.join(name)))
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
/// by that name. Each exclusion is expected where it really is instead
/// ([`with_real_paths`]), and the walk reaches it under its own name.
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
/// - A symlink created, after the walk, where an exclusion has a missing
///   part: that part is expected to become a directory ([`real_path`]), so
///   the exclusion is placed beneath it, and what is written through the link
///   lands at its target, which may be granted. Like the swap above, it
///   matters only for a protected path under a directory a build can write.
/// - An exclusion near `PATH_MAX`: a lookup failing with `ENAMETOOLONG` is
///   taken as a missing name, so a link beyond it is not followed, while the
///   confined process can still reach it by relative lookups. Only a
///   deployment makes a layout that deep.
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

    /// A path is placed where it really is through dangling links, chained or
    /// relative, and kept as written when they loop.
    #[test]
    fn places_a_path_behind_dangling_links_where_it_will_be() {
        let dir = scratch("aurcache-sandbox-real");
        std::fs::create_dir_all(dir.join("store")).unwrap();
        std::fs::create_dir_all(dir.join("w")).unwrap();
        std::os::unix::fs::symlink("../store/key", dir.join("w/key")).unwrap();
        std::os::unix::fs::symlink("w/key", dir.join("chain")).unwrap();
        std::os::unix::fs::symlink("store/data", dir.join("data")).unwrap();
        std::os::unix::fs::symlink("loop-b", dir.join("loop-a")).unwrap();
        std::os::unix::fs::symlink("loop-a", dir.join("loop-b")).unwrap();

        assert_eq!(
            with_real_paths(vec![
                dir.join("w/key"),
                dir.join("chain"),
                dir.join("data/ca/key"),
                dir.join("loop-a/key"),
            ]),
            vec![
                dir.join("store/key"),
                dir.join("store/key"),
                dir.join("store/data/ca/key"),
                dir.join("loop-a/key"),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A `..` after a missing part climbs from where that part will be, a
    /// directory: `gone/..` is the directory `gone` would be created in, and
    /// a further `..` climbs above the part that exists, to `/` at most.
    #[test]
    fn places_a_path_with_a_dotdot_after_a_missing_part_where_it_will_be() {
        let dir = scratch("aurcache-sandbox-dotdot");
        std::fs::create_dir_all(dir.join("w/store")).unwrap();
        std::os::unix::fs::symlink("gone/../store/key", dir.join("w/k")).unwrap();
        std::os::unix::fs::symlink("gone/a/../../../up", dir.join("w/up")).unwrap();
        std::os::unix::fs::symlink("gone/sub", dir.join("w/l")).unwrap();

        assert_eq!(
            with_real_paths(vec![
                dir.join("w/k"),
                dir.join("w/up"),
                dir.join("w/l/../key"),
                dir.join("w/gone/../store/./new"),
                PathBuf::from("/aurcache-sandbox-gone/../../x"),
            ]),
            vec![
                dir.join("w/store/key"),
                dir.join("up"),
                dir.join("w/gone/key"),
                dir.join("w/store/new"),
                PathBuf::from("/x"),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Paths through links of every kind -- absolute, relative, dangling,
    /// chained, with `.` and `..` in them, looping -- are predicted where the
    /// kernel puts them once their missing parts are created as directories,
    /// and a path through more links than the kernel follows is predicted
    /// nowhere. A path the kernel cannot resolve otherwise, or that leaves the
    /// layout, is skipped.
    #[test]
    fn predicts_where_the_kernel_resolves_a_path_once_created() {
        use std::os::unix::ffi::OsStringExt;

        const RANDOM: usize = 200;
        const NAMES: &[&[u8]] = &[
            b"gone", b"gone2", b"x", b"store", b"S", b"R", b"D", b"DA", b"T", b"U", b"TOP", b"DD",
            b"DS", b"..", b"..", b"..", b".", b"", b"n\xff", b"F", b"L1", b"a", b"b",
        ];
        let fixed: &[&[u8]] = &[
            // A link after a missing part cancelled by `..`, and a dangling
            // link's target spelled that way.
            b"gone/../S/x",
            b"gone/../S",
            b"gone/../R/x",
            b"gone/../D/x",
            b"gone/../store/x",
            b"DS/x",
            b"DD/x",
            b"gone/../../w/S/x",
            b"gone/a/../../T/x",
            b"U/gone/../DS/x",
            // As many links as the kernel follows, and one more.
            b"N40/x",
            b"N41/x",
        ];
        // xorshift64, seeded: the same paths on every run.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut below = |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };
        let random = (0..RANDOM).map(|_| {
            let mut rel = Vec::new();
            for k in 0..1 + below(6) {
                if k > 0 {
                    rel.push(b'/');
                }
                rel.extend_from_slice(NAMES[below(NAMES.len())]);
            }
            if below(5) == 0 {
                rel.push(b'/');
            }
            rel
        });

        let dir = scratch("aurcache-sandbox-predict");
        // Deep enough for most `..`s to stay inside `dir`.
        let base = (0..8).fold(dir.clone(), |base, l| base.join(format!("l{l}")));
        prediction_layout(&base);
        let layout = tree(&dir);
        let mut compared = 0;
        let cases: Vec<Vec<u8>> = fixed.iter().map(|rel| rel.to_vec()).chain(random).collect();
        for rel in &cases {
            let mut path = base.join("w/").into_os_string().into_vec();
            path.extend_from_slice(rel);
            let path = PathBuf::from(std::ffi::OsString::from_vec(path));

            let predicted = real_path(&path);
            let mut created = Vec::new();
            let made = create_missing(&path, &dir, 0, &mut created);
            let too_many_links = std::fs::metadata(&path)
                .is_err_and(|error| error.raw_os_error() == Some(libc::ELOOP));
            if too_many_links {
                assert_eq!(predicted, None, "{}", path.display());
                compared += 1;
            } else if made.is_ok()
                && let Ok(actual) = std::fs::canonicalize(&path)
                && actual.starts_with(&dir)
            {
                assert_eq!(predicted.as_ref(), Some(&actual), "{}", path.display());
                compared += 1;
            }
            for created in created.iter().rev() {
                std::fs::remove_dir(created).unwrap();
            }
            assert_eq!(tree(&dir), layout, "{} changed the layout", path.display());
        }
        assert!(compared > cases.len() / 2, "only {compared} compared");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The links [`predicts_where_the_kernel_resolves_a_path_once_created`]
    /// walks through, in `base/w`.
    fn prediction_layout(base: &Path) {
        use std::os::unix::fs::symlink;
        let real = base.join("real");
        let w = base.join("w");
        std::fs::create_dir_all(w.join("store")).unwrap();
        std::fs::create_dir_all(real.join("a/b")).unwrap();
        std::fs::write(w.join("F"), "").unwrap();
        symlink(real.join("a/b"), w.join("S")).unwrap();
        symlink("../real/a", w.join("R")).unwrap();
        symlink("gone2/deep", w.join("D")).unwrap();
        symlink(real.join("gone3"), w.join("DA")).unwrap();
        symlink("./store/", w.join("T")).unwrap();
        symlink("../w/store/..", w.join("U")).unwrap();
        symlink(base, w.join("TOP")).unwrap();
        symlink("g4/../g5/", w.join("DD")).unwrap();
        symlink("../w/gone6/../S", w.join("DS")).unwrap();
        symlink("L1", w.join("L2")).unwrap();
        symlink("L2", w.join("L1")).unwrap();
        // `Nk` reaches `store` through k links.
        symlink("store", w.join("N1")).unwrap();
        for k in 2..=41 {
            symlink(format!("N{}", k - 1), w.join(format!("N{k}"))).unwrap();
        }
    }

    /// Every path under `dir`, links not followed, sorted.
    fn tree(dir: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let mut todo = vec![dir.to_owned()];
        while let Some(dir) = todo.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    todo.push(entry.path());
                }
                paths.push(entry.path());
            }
        }
        paths.sort();
        paths
    }

    /// Create every missing part of `path` as a directory, where the kernel
    /// puts it: a dangling link's at its target. Nothing is created outside
    /// `within`. Each directory created is added to `created`, parents first,
    /// whether or not the whole of `path` could be.
    fn create_missing(
        path: &Path,
        within: &Path,
        depth: usize,
        created: &mut Vec<PathBuf>,
    ) -> std::io::Result<()> {
        let refuse = |why: String| Err(std::io::Error::other(why));
        if depth > 40 {
            return refuse("a link loop".into());
        }
        let mut prefix = PathBuf::new();
        for part in path.components() {
            prefix.push(part);
            if prefix.metadata().is_ok() {
                continue;
            }
            if prefix.symlink_metadata().is_ok() {
                let target = prefix.parent().unwrap().join(std::fs::read_link(&prefix)?);
                create_missing(&target, within, depth + 1, created)?;
                prefix.metadata()?;
                continue;
            }
            let Component::Normal(_) = part else {
                return refuse(format!("{} cannot be created", prefix.display()));
            };
            if !std::fs::canonicalize(prefix.parent().unwrap())?.starts_with(within) {
                return refuse(format!("{} is outside", prefix.display()));
            }
            std::fs::create_dir(&prefix)?;
            created.push(prefix.clone());
        }
        Ok(())
    }
}
