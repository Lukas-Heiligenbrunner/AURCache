//! The build profile: confine the PKGBUILD code a build runs on the worker,
//! outside its chroot, to that build's own directories.
//!
//! ```text
//! aurcache-sandbox --allow-build-env -- PROGRAM [ARGS]...
//! ```
//!
//! # Why this exists
//!
//! `makechrootpkg` downloads sources **outside** the chroot: its
//! `download_sources()` runs `makepkg --verifysource` on the worker as the
//! build user, and it reads the package's metadata with a plain
//! `source PKGBUILD`. Sourcing a PKGBUILD executes it, so every build runs
//! attacker-supplied code on the worker, outside any chroot, as a user that can
//! write every other build's files -- another package's cached sources, the
//! shared GnuPG keyring, and concurrent jobs' `PKGBUILD`s. Rewriting a
//! neighbour's PKGBUILD also rewrites the checksums it declares, so integrity
//! checks are no defence: they are claims made by the file under attack.
//!
//! # Why only those two sites
//!
//! Landlock refuses `mount(2)` outright for a restricted process -- verified
//! against every combination of handled access rights -- so nothing confined
//! this way can run `arch-nspawn`. The restriction therefore cannot be applied
//! to `makechrootpkg` as a whole; the patched copy the worker runs
//! (`packaging/patch-makechrootpkg.py`) wraps exactly the two sites that
//! execute PKGBUILD code without mounting, and the chroot build proceeds in a
//! process this policy never touched.
//!
//! # Policy
//!
//! - **Writes**: `SRCDEST`, `BUILDDIR` and the working directory, nothing
//!   else. That is right for a build and wrong anywhere else, which is why the
//!   flag says so rather than it being a default.
//! - **Reads**: everything except the paths listed in [`PROTECTED_PATHS_FILE`]
//!   -- the worker's own secrets. A build legitimately reads most of the
//!   filesystem, and reading is not how one build corrupts another; what must
//!   stay secret is short and known. With no list, reads are not restricted.
//!
//! What this cannot do is protect a secret already in the process environment:
//! a PKGBUILD reads it without touching the filesystem. makechrootpkg reaches
//! here through `sudo`, which passes on only what it is told to preserve.

use landlock::{ABI, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr};
use landlock::{RulesetCreatedAttr, RulesetStatus};
use std::convert::Infallible;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Environment variables naming a directory this build may write.
///
/// `SRCDEST` is the per-package source cache and `BUILDDIR` the per-run work
/// directory; `makechrootpkg` sets both immediately before invoking `makepkg`,
/// so a PKGBUILD cannot influence them.
const WRITABLE_FROM_ENV: [&str; 2] = ["SRCDEST", "BUILDDIR"];

/// File listing paths a build may not read, one per line.
///
/// A file rather than an environment variable, because `makechrootpkg` reaches
/// the build through `sudo --preserve-env=GNUPGHOME,SSH_AUTH_SOCK`, which
/// strips everything else -- an env-configured policy silently arrives empty,
/// which looks exactly like a policy that decided to protect nothing.
///
/// It also keeps deployment paths out of the makechrootpkg patch, so that stays
/// a pure mechanical transform.
const PROTECTED_PATHS_FILE: &str = "/etc/aurcache/sandbox-protected";

const USAGE: &str = "usage: aurcache-sandbox --allow-build-env -- PROGRAM [ARGS]...\n       \
                     aurcache-sandbox parse --pkgbuild FILE [--private PATH]... [--network] \
                     -- PROGRAM [ARGS]...";

struct Request {
    program: OsString,
    args: Vec<OsString>,
}

/// Why a build was not run.
enum Failure {
    /// The protected-path list names a path under a directory that does not
    /// exist.
    BadPolicy(String),
    /// Landlock could not be applied in full.
    Unconfined(String),
    /// The program could not be executed.
    Exec(String),
}

pub fn main(args: impl Iterator<Item = OsString>) -> ExitCode {
    let request = match parse_args(args) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("aurcache-sandbox: {e}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&request) {
        Ok(never) => match never {},
        Err(Failure::BadPolicy(e)) => {
            eprintln!("aurcache-sandbox: {e}");
            ExitCode::from(2)
        }
        Err(Failure::Unconfined(e)) => {
            // Fail closed. Running unconfined would silently reinstate the very
            // cross-build write channel this exists to remove, and it would
            // look exactly like success.
            eprintln!("aurcache-sandbox: refusing to run unconfined: {e}");
            ExitCode::from(1)
        }
        Err(Failure::Exec(e)) => {
            eprintln!("aurcache-sandbox: {e}");
            ExitCode::from(127)
        }
    }
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Request, String> {
    if args.next().is_none_or(|arg| arg != "--allow-build-env") {
        return Err("expected `--allow-build-env`".into());
    }
    if args.next().is_none_or(|arg| arg != "--") {
        return Err("expected `--` after `--allow-build-env`".into());
    }
    let program = args.next().ok_or("missing PROGRAM after `--`")?;
    Ok(Request {
        program,
        args: args.collect(),
    })
}

/// Confine this process and exec the program; returns only on failure.
fn run(request: &Request) -> Result<Infallible, Failure> {
    let protected = protected_paths(Path::new(PROTECTED_PATHS_FILE));
    check_parents(&protected).map_err(Failure::BadPolicy)?;
    let read = if protected.is_empty() {
        Vec::new()
    } else {
        crate::read_grants_excluding(&protected, crate::list_dir)
    };
    restrict(&writable_dirs(), &read).map_err(|e| Failure::Unconfined(e.to_string()))?;

    let err = std::process::Command::new(&request.program)
        .args(&request.args)
        .exec();
    Err(Failure::Exec(format!(
        "cannot execute {}: {err}",
        request.program.to_string_lossy()
    )))
}

/// Refuse a protected path whose parent does not exist.
///
/// The path itself need not exist -- a worker protects its identity file
/// before enrollment has written it, and the walk is path-based, so a name
/// absent from the listing is simply never granted, and a file created there
/// later inherits that (its parent is an exclusion ancestor, so it is never
/// blanket-granted).
///
/// The *parent* must exist, though. A typo there -- `/vr/lib/...` for
/// `/var/lib/...` -- would leave the real directory granted wholesale,
/// producing a policy that reads as protection and is not.
fn check_parents(protected: &[PathBuf]) -> Result<(), String> {
    for path in protected {
        let parent = path.parent().unwrap_or_else(|| Path::new("/"));
        if !parent.is_dir() {
            return Err(format!(
                "parent of protected path {} does not exist ({}); \
                 refusing to build a policy that would leave it granted",
                path.display(),
                parent.display()
            ));
        }
    }
    Ok(())
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

/// Apply the Landlock policy to this process (inherited by the exec'd child).
/// `read` empty leaves reads unrestricted.
fn restrict(allow: &[PathBuf], read: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    // Writing implies being able to move things around inside a writable tree,
    // and makepkg renames across directories as a matter of course (tar
    // extraction, breezy staging a pack into `packs/`, ...). Landlock only
    // permits such renames when it explicitly grants `Refer`, a right added in
    // ABI V2 (kernel 5.19); a V1 ruleset makes every one of them fail with
    // EXDEV. V2 is therefore the floor, not V1.
    let abi = ABI::V2;
    let write = AccessFs::from_write(abi);
    let read_only = AccessFs::from_read(abi);

    // Handling reads with nothing to keep from them would deny the build every
    // library and config it legitimately needs.
    let handled = if read.is_empty() {
        write
    } else {
        write | read_only
    };
    // The floor is V2, whose shortfalls the `FullyEnforced` check below
    // catches anyway.
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(handled)?
        .create()?;

    for path in read {
        ruleset = crate::grant(ruleset, path, read_only)?;
    }

    for path in allow {
        ruleset = crate::grant(ruleset, path, handled)?;
    }

    // makepkg and the PKGBUILDs it sources redirect to and from /dev/null
    // constantly; denying it breaks the build without protecting anything.
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

    fn args(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// The one form the patched makechrootpkg uses.
    #[test]
    fn accepts_the_build_invocation() {
        let request = parse_args(args(&[
            "--allow-build-env",
            "--",
            "makepkg",
            "--verifysource",
        ]))
        .unwrap();
        assert_eq!(request.program, "makepkg");
        assert_eq!(request.args, vec![OsString::from("--verifysource")]);
    }

    /// Anything else is a usage error, never a differently-shaped policy.
    #[test]
    fn refuses_any_other_invocation() {
        for list in [
            &[][..],
            &["--", "makepkg"],
            &["--allow-build-env", "makepkg"],
            &["--allow-build-env", "--"],
            &["--allow", "/tmp", "--allow-build-env", "--", "makepkg"],
        ] {
            assert!(parse_args(args(list)).is_err(), "{list:?}");
        }
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

    /// The write set must carry `Refer` (ABI V2+): without it every
    /// cross-directory rename inside a writable tree fails with EXDEV, and
    /// builds rename constantly. This keeps the ABI from being downgraded back
    /// to V1 as a "compatibility" measure.
    #[test]
    fn the_write_set_carries_refer() {
        assert!(AccessFs::from_write(ABI::V2).contains(AccessFs::Refer));
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

    /// A typo in a protected path's directory must refuse, not leave the real
    /// directory granted wholesale.
    #[test]
    fn refuses_a_protected_path_under_a_missing_directory() {
        assert!(check_parents(&[PathBuf::from("/nonexistent-aurcache-dir/secret")]).is_err());
        assert!(check_parents(&[PathBuf::from("/etc/not-yet-written")]).is_ok());
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
