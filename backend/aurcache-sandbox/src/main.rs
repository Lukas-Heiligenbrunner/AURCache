//! Confine `makepkg --verifysource` to one build's own directories.
//!
//! # Why this exists
//!
//! `makechrootpkg` downloads sources **outside** the chroot: its
//! `download_sources()` runs `makepkg --verifysource` on the worker as the
//! build user. `makepkg` sources the PKGBUILD to learn what to fetch, and
//! sourcing a PKGBUILD executes it. So every build runs attacker-supplied code
//! on the worker, outside any chroot, as a user that can write every other
//! build's files — another package's cached sources, the shared GnuPG keyring,
//! and concurrent jobs' `PKGBUILD`s. Rewriting a neighbour's PKGBUILD also
//! rewrites the checksums it declares, so integrity checks are no defence:
//! they are claims made by the file under attack.
//!
//! This wrapper applies a Landlock policy and then execs the real `makepkg`,
//! so that code runs with write access to nothing but its own build.
//!
//! # Why a separate binary, exec'd from a PATH shim
//!
//! Landlock refuses `mount(2)` outright for a restricted process — verified
//! against every combination of handled access rights — so nothing confined
//! this way can run `arch-nspawn`. The restriction therefore cannot be applied
//! to `makechrootpkg` as a whole; it has to wrap only the phase that executes
//! PKGBUILD code without mounting. `download_sources()` invokes `makepkg`
//! unqualified and is the only host-side call that does, so shadowing it on
//! `PATH` confines exactly that phase and nothing else. The chroot build then
//! proceeds unrestricted in a process this policy never touched.
//!
//! # Policy
//!
//! Reads are unrestricted; makepkg legitimately reads the chroot's
//! `makepkg.conf`, the package cache and much of `/usr`, and read access is
//! not what lets one build corrupt another. Writes are denied everywhere
//! except directories named explicitly.
//!
//! Nothing is writable by default. `--allow-build-env` opts into the worker's
//! set (`SRCDEST`, `BUILDDIR` and the working directory), which is correct
//! there and wrong anywhere else: the server parses PKGBUILDs with its working
//! directory on `/app`, alongside the database and the package repository.
//! Granting the working directory implicitly would hand a PKGBUILD the repo.
//!
//! Reads are unrestricted **until the first `--read` or `--read-except`**,
//! after which only the granted paths are readable.
//!
//! `--read-except` is the ergonomic form: Landlock grants can only be positive,
//! so "everything except `/app`" is expressed by granting every sibling along
//! the path to `/app` and never granting `/app` itself. That is the right shape
//! for this problem — enumerating what a build legitimately reads is large,
//! open-ended, and a miss silently breaks builds, whereas enumerating what must
//! stay secret is short, known, and a miss is the only real risk. The worker wants the permissive form — a build
//! legitimately reads most of the filesystem, and reading is not how one build
//! corrupts another. The server wants the strict form, because there the threat
//! is a PKGBUILD reading the package database or the built packages.
//!
//! What this cannot do is protect a secret already in the process environment:
//! a PKGBUILD reads `$DB_PWD` without touching the filesystem. Callers must
//! scrub the environment themselves (see packaging/alpm-pkgbuild-bridge-wrapper).

use landlock::{
    ABI, AccessFs, BitFlags, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreated,
    RulesetCreatedAttr, RulesetStatus,
};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// Environment variables naming a directory this build may write.
///
/// `SRCDEST` is the per-package source cache and `BUILDDIR` the per-run work
/// directory; `makechrootpkg` sets both immediately before invoking `makepkg`,
/// so a PKGBUILD cannot influence them.
const WRITABLE_FROM_ENV: [&str; 2] = ["SRCDEST", "BUILDDIR"];

/// File listing paths kept unreadable under `--allow-build-env`, one per line.
///
/// A file rather than an environment variable, because `makechrootpkg` reaches
/// the build through `sudo --preserve-env=GNUPGHOME,SSH_AUTH_SOCK`, which
/// strips everything else — an env-configured policy silently arrives empty,
/// which looks exactly like a policy that decided to protect nothing.
///
/// It also keeps deployment paths out of the makechrootpkg patch, so that stays
/// a pure mechanical transform.
const PROTECTED_PATHS_FILE: &str = "/etc/aurcache/sandbox-protected";

fn main() -> std::process::ExitCode {
    let mut allow: Vec<PathBuf> = Vec::new();
    let mut read: Vec<PathBuf> = Vec::new();
    let mut read_except: Vec<PathBuf> = Vec::new();
    let mut build_env = false;
    let mut args = std::env::args_os().skip(1).peekable();

    while let Some(arg) = args.peek() {
        match arg.to_str() {
            Some("--allow-build-env") => {
                build_env = true;
                args.next();
            }
            Some("--allow") => {
                args.next();
                match args.next() {
                    Some(dir) => allow.push(PathBuf::from(dir)),
                    None => {
                        eprintln!("aurcache-sandbox: --allow needs a directory");
                        return std::process::ExitCode::from(2);
                    }
                }
            }
            Some("--read") => {
                args.next();
                match args.next() {
                    Some(dir) => read.push(PathBuf::from(dir)),
                    None => {
                        eprintln!("aurcache-sandbox: --read needs a directory");
                        return std::process::ExitCode::from(2);
                    }
                }
            }
            Some("--read-except") => {
                args.next();
                match args.next() {
                    Some(dir) => read_except.push(PathBuf::from(dir)),
                    None => {
                        eprintln!("aurcache-sandbox: --read-except needs a directory");
                        return std::process::ExitCode::from(2);
                    }
                }
            }
            Some("--") => {
                args.next();
                break;
            }
            _ => break,
        }
    }

    let argv: Vec<_> = args.collect();
    let Some((program, rest)) = argv.split_first() else {
        eprintln!(
            "usage: aurcache-sandbox [--allow DIR]... [--allow-build-env] [--] <command> [args...]"
        );
        return std::process::ExitCode::from(2);
    };

    if build_env {
        allow.extend(writable_dirs());
        read_except.extend(protected_paths(Path::new(PROTECTED_PATHS_FILE)));
    }

    if !read_except.is_empty() {
        // The excluded path itself need not exist — a worker protects its
        // identity file before enrollment has written it, and the walk is
        // path-based, so a name absent from the listing is simply never
        // granted, and a file created there later inherits that (its parent is
        // an exclusion ancestor, so it is never blanket-granted).
        //
        // The *parent* must exist, though. A typo there — `/vr/lib/...` for
        // `/var/lib/...` — would leave the real directory granted wholesale,
        // producing a policy that reads as protection and is not.
        for path in &read_except {
            let parent = path.parent().unwrap_or_else(|| Path::new("/"));
            if !parent.is_dir() {
                eprintln!(
                    "aurcache-sandbox: parent of --read-except {} does not exist ({}); \
                     refusing to build a policy that would leave it granted",
                    path.display(),
                    parent.display()
                );
                return std::process::ExitCode::from(2);
            }
        }
        read.extend(read_grants_excluding(&read_except, |dir| {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return Vec::new();
            };
            entries.flatten().map(|e| e.path()).collect()
        }));
    }

    if let Err(e) = restrict(&allow, &read) {
        // Fail closed. Running unconfined would silently reinstate the very
        // cross-build write channel this exists to remove, and it would look
        // exactly like success.
        eprintln!("aurcache-sandbox: refusing to run unconfined: {e}");
        return std::process::ExitCode::from(1);
    }

    let err = std::process::Command::new(program).args(rest).exec();
    eprintln!(
        "aurcache-sandbox: cannot execute {}: {err}",
        program.to_string_lossy()
    );
    std::process::ExitCode::from(127)
}

/// Collect the directories this build is allowed to write.
fn writable_dirs() -> Vec<PathBuf> {
    resolve_writable_dirs(
        |key| std::env::var_os(key).map(PathBuf::from),
        std::env::current_dir().ok(),
    )
}

/// Pure form of [`writable_dirs`], so the policy inputs can be tested without
/// mutating the process environment.
///
/// `cwd` is the extracted package directory: makepkg's working directory,
/// holding the PKGBUILD it is about to source. It is writable because makepkg
/// writes logs there and VCS packages rewrite the PKGBUILD in place.
fn resolve_writable_dirs(
    lookup: impl Fn(&str) -> Option<PathBuf>,
    cwd: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = WRITABLE_FROM_ENV
        .iter()
        .filter_map(|key| lookup(key))
        .filter(|v| !v.as_os_str().is_empty())
        .collect();
    dirs.extend(cwd);
    dirs
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

/// Read the deployment's protected-path list, if it has one.
///
/// A missing file means "protect nothing", which is the correct default for a
/// worker with no secrets on disk. Blank lines and `#` comments are ignored.
fn protected_paths(file: &Path) -> Vec<PathBuf> {
    let Ok(contents) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(PathBuf::from)
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

/// Apply the Landlock policy to this process (inherited by the exec'd child).
fn restrict(allow: &[PathBuf], read: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    // V1 is the floor every Landlock kernel supports; asking for more would
    // refuse to run on kernels that can still enforce what we actually need.
    let abi = ABI::V1;
    let write = AccessFs::from_write(abi);
    let read_only = AccessFs::from_read(abi);

    // Handling reads at all is opt-in: doing so unconditionally would deny the
    // worker every library and config its build legitimately needs.
    let handled = if read.is_empty() {
        write
    } else {
        write | read_only
    };
    let mut ruleset = Ruleset::default().handle_access(handled)?.create()?;

    for path in read {
        ruleset = grant(ruleset, path, read_only)?;
    }

    for path in allow {
        ruleset = grant(ruleset, path, handled)?;
    }

    // makepkg and the PKGBUILDs it sources redirect to and from /dev/null
    // constantly; denying it breaks the parse without protecting anything.
    //
    // The grant is intersected with what the ruleset handles: Landlock rejects
    // a rule granting a right the ruleset never took responsibility for, so
    // asking for `ReadFile` when only writes are handled is a hard error.
    let devnull = (AccessFs::WriteFile | AccessFs::ReadFile) & handled;
    if !devnull.is_empty()
        && let Ok(fd) = PathFd::new("/dev/null")
    {
        ruleset = ruleset.add_rule(PathBeneath::new(fd, devnull))?;
    }

    match ruleset.restrict_self()?.ruleset {
        RulesetStatus::FullyEnforced => Ok(()),
        // Partial enforcement means the kernel silently dropped part of the
        // policy. Treat it as failure: a policy with unknown holes is worse
        // than a loud refusal, because it reads as protection.
        status => Err(format!("Landlock not fully enforced ({status:?})").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<PathBuf> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| map.get(key).map(PathBuf::from)
    }

    #[test]
    fn grants_the_build_its_own_directories() {
        let dirs = resolve_writable_dirs(
            lookup_from(&[("SRCDEST", "/cache/srcdest/demo"), ("BUILDDIR", "/tmp/wd")]),
            Some(PathBuf::from("/work/42/demo")),
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/cache/srcdest/demo"),
                PathBuf::from("/tmp/wd"),
                PathBuf::from("/work/42/demo"),
            ]
        );
    }

    /// An unset or blank variable must not become a rule. `PathBeneath` on ""
    /// would fail, but an empty value silently resolving to the current
    /// directory would be worse: it would widen the policy without saying so.
    #[test]
    fn ignores_unset_and_blank_variables() {
        let dirs = resolve_writable_dirs(lookup_from(&[("SRCDEST", "")]), None);
        assert!(dirs.is_empty());
    }

    /// With no environment at all the policy must still be a policy — writable
    /// nowhere except the working directory — rather than degrading to
    /// unrestricted.
    #[test]
    fn without_environment_only_the_working_directory_is_writable() {
        let dirs = resolve_writable_dirs(lookup_from(&[]), Some(PathBuf::from("/work/42/demo")));
        assert_eq!(dirs, vec![PathBuf::from("/work/42/demo")]);
    }

    /// The whole point of `--read-except`: a short list of secrets stays
    /// unreadable while everything else is granted, without enumerating the
    /// filesystem by hand.
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

    #[test]
    fn reads_protected_paths_ignoring_comments_and_blanks() {
        let dir = std::env::temp_dir().join("aurcache-sandbox-test-protected");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("list");
        std::fs::write(
            &file,
            "# secrets\n/var/lib/aurcache-worker/secrets\n\n  /etc/aurcache/identity  \n",
        )
        .unwrap();
        assert_eq!(
            protected_paths(&file),
            vec![
                PathBuf::from("/var/lib/aurcache-worker/secrets"),
                PathBuf::from("/etc/aurcache/identity"),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A missing list must mean "nothing to protect", not a crash — a worker
    /// with no on-disk secrets is a normal deployment.
    #[test]
    fn a_missing_protected_list_protects_nothing() {
        assert!(protected_paths(Path::new("/nonexistent/aurcache/list")).is_empty());
    }
}
