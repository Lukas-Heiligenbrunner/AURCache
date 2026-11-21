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
//! process this policy never touched. The worker's own `makepkg
//! --printsrcinfo`, which reads the dependencies a build installs, runs under
//! it too (`aurcache_worker::build_deps`).
//!
//! # Policy
//!
//! - **Writes**: `SRCDEST`, `BUILDDIR` and the working directory, nothing
//!   else. That is right for a build and wrong anywhere else, which is why the
//!   flag says so rather than it being a default.
//! - **Reads**: everything except the paths listed in [`PROTECTED_PATHS_FILE`]
//!   -- the worker's own secrets. A build legitimately reads most of the
//!   filesystem, and reading is not how one build corrupts another; what must
//!   stay secret is short and known. With no list, reads are not restricted;
//!   a list that exists but cannot be read refuses the build.
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
                     -- PROGRAM [ARGS]...\n       \
                     aurcache-sandbox offline -- PROGRAM [ARGS]...";

struct Request {
    program: OsString,
    args: Vec<OsString>,
}

/// Why a build was not run.
enum Failure {
    /// The protected-path list cannot be read, or names a path under a
    /// directory that does not exist.
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
    let protected = protected_paths(Path::new(PROTECTED_PATHS_FILE)).map_err(Failure::BadPolicy)?;
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
/// producing a policy that reads as protection and is not. It is checked
/// where the path really is ([`protected_paths`]), so a dangling link into a
/// missing directory is refused the same way.
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
///
/// A file that exists but cannot be read is an error, never an empty list: a
/// policy at `0600 root` reads as missing to the build user this runs as, and
/// taking that for "protect nothing" would leave every secret it lists
/// readable while looking exactly like a confined build.
///
/// Each listed path is protected where it really is, whether or not it exists
/// yet ([`crate::with_real_paths`]).
fn protected_paths(file: &Path) -> Result<Vec<PathBuf>, String> {
    let contents = match read_policy(file) {
        Ok(contents) => contents,
        // Missing only when there is nothing by its name: a symlink to nothing,
        // on the way to it or in its place, is a list that cannot be read.
        Err(Unreadable::Io(e)) if e.kind() == std::io::ErrorKind::NotFound && dangles(file) => {
            return Err(Unreadable::Dangling.refusal(file));
        }
        Err(Unreadable::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(unreadable) => return Err(unreadable.refusal(file)),
    };
    Ok(crate::with_real_paths(
        contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(PathBuf::from)
            .collect(),
    ))
}

/// The most a protected-path list may hold: far more than any list of paths
/// needs, and little enough to read whole.
const MAX_POLICY_BYTES: u64 = 1 << 20;

/// Why the protected-path list could not be read.
enum Unreadable {
    /// Something other than a regular file is in its place, such as a
    /// directory, a FIFO or a device.
    NotAFile,
    /// It holds more than [`MAX_POLICY_BYTES`].
    TooLarge,
    /// It is a symlink to nothing, or is reached through one.
    Dangling,
    /// Opening or reading it failed.
    Io(std::io::Error),
}

impl Unreadable {
    /// The message refusing the build, with what to do about it.
    fn refusal(&self, file: &Path) -> String {
        let cannot = format!("cannot read the protected-path list {}", file.display());
        let refusing = "refusing to build without knowing what it protects";
        match self {
            Self::NotAFile => {
                format!("{cannot} (not a regular file); {refusing}: replace it with a regular file")
            }
            Self::TooLarge => format!(
                "{cannot} (larger than {MAX_POLICY_BYTES} bytes); {refusing}: \
                 list only the paths a build must never read"
            ),
            Self::Dangling => format!(
                "{cannot} (a symlink to nothing, in its place or on the way to it); \
                 {refusing}: fix the symlink that leads nowhere"
            ),
            Self::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => format!(
                "{cannot} ({e}); {refusing}: make it readable by the build user \
                 (it lists paths, not secrets)"
            ),
            Self::Io(e) => format!("{cannot} ({e}); {refusing}"),
        }
    }
}

/// Read the protected-path list whole, if it is a regular file.
///
/// Opened without blocking and checked before reading: a FIFO in its place
/// would block the build forever, and a device such as `/dev/zero` never ends.
fn read_policy(file: &Path) -> Result<String, Unreadable> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(file)
        .map_err(Unreadable::Io)?;
    if !opened.metadata().map_err(Unreadable::Io)?.is_file() {
        return Err(Unreadable::NotAFile);
    }
    let mut contents = String::new();
    opened
        .take(MAX_POLICY_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(Unreadable::Io)?;
    if contents.len() as u64 > MAX_POLICY_BYTES {
        return Err(Unreadable::TooLarge);
    }
    Ok(contents)
}

/// Whether `path`, or a directory on the way to it, is a symlink that cannot
/// be resolved.
fn dangles(path: &Path) -> bool {
    path.ancestors().any(|path| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
            && std::fs::canonicalize(path).is_err()
    })
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
            "# secrets\n/var/lib/aurcache-worker/ssh\n\n  /etc/aurcache/identity  \n",
        )
        .unwrap();
        assert_eq!(
            protected_paths(&file).unwrap(),
            vec![
                PathBuf::from("/var/lib/aurcache-worker/ssh"),
                PathBuf::from("/etc/aurcache/identity"),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A listed path behind a symlink is protected where it really is.
    #[test]
    fn a_listed_symlink_is_protected_at_its_real_path() {
        let dir =
            std::env::temp_dir().join(format!("aurcache-sandbox-test-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stamped = dir.join("secret/..2026_11_17");
        std::fs::create_dir_all(&stamped).unwrap();
        std::fs::write(stamped.join("key"), "secret").unwrap();
        std::os::unix::fs::symlink("..2026_11_17", dir.join("secret/..data")).unwrap();
        std::os::unix::fs::symlink("..data/key", dir.join("secret/key")).unwrap();
        let file = dir.join("list");
        std::fs::write(&file, format!("{}\n", dir.join("secret/key").display())).unwrap();

        let protected = protected_paths(&file).unwrap();
        assert_eq!(
            protected,
            vec![std::fs::canonicalize(stamped.join("key")).unwrap()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A listed path not written yet, under a linked directory, is protected
    /// where it will really be: the walk would otherwise grant it there once
    /// it is created.
    #[test]
    fn a_missing_path_under_a_linked_directory_is_protected_at_its_real_path() {
        let dir = std::env::temp_dir().join(format!(
            "aurcache-sandbox-test-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink("real", dir.join("alias")).unwrap();
        let file = dir.join("list");
        std::fs::write(&file, format!("{}\n", dir.join("alias/identity").display())).unwrap();

        let protected = protected_paths(&file).unwrap();
        assert!(check_parents(&protected).is_ok(), "{protected:?}");
        assert_eq!(
            protected,
            vec![
                std::fs::canonicalize(dir.join("real"))
                    .unwrap()
                    .join("identity"),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A listed path that is a dangling link is protected where its target
    /// will be, not only at the link: the walk would grant the target's
    /// directory, and the target with it once written. A link into a missing
    /// directory is refused like a missing parent.
    #[test]
    fn a_dangling_symlink_is_protected_where_its_target_will_be() {
        let dir = std::env::temp_dir().join(format!(
            "aurcache-sandbox-test-dangling-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("w")).unwrap();
        std::fs::create_dir_all(dir.join("store")).unwrap();
        std::os::unix::fs::symlink("../store/worker-key.pem", dir.join("w/worker-key.pem"))
            .unwrap();
        std::os::unix::fs::symlink("../gone/ssh", dir.join("w/ssh")).unwrap();
        let file = dir.join("list");
        std::fs::write(
            &file,
            format!("{}\n", dir.join("w/worker-key.pem").display()),
        )
        .unwrap();

        let protected = protected_paths(&file).unwrap();
        assert!(check_parents(&protected).is_ok(), "{protected:?}");
        assert_eq!(
            protected,
            vec![
                std::fs::canonicalize(dir.join("store"))
                    .unwrap()
                    .join("worker-key.pem"),
            ]
        );

        std::fs::write(
            &file,
            format!("{}\n", dir.join("w/ssh/id_ed25519").display()),
        )
        .unwrap();
        assert!(check_parents(&protected_paths(&file).unwrap()).is_err());

        // A `..` after a missing part: `gone/..` is `w` once `gone` exists.
        std::fs::create_dir_all(dir.join("w/store")).unwrap();
        std::os::unix::fs::symlink("gone/../store/key", dir.join("w/k")).unwrap();
        std::fs::write(&file, format!("{}\n", dir.join("w/k").display())).unwrap();
        let protected = protected_paths(&file).unwrap();
        assert!(check_parents(&protected).is_ok(), "{protected:?}");
        assert_eq!(
            protected,
            vec![
                std::fs::canonicalize(dir.join("w/store"))
                    .unwrap()
                    .join("key"),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A missing list must mean "nothing to protect", not a crash — a worker
    /// with no on-disk secrets is a normal deployment.
    #[test]
    fn a_missing_protected_list_protects_nothing() {
        assert_eq!(
            protected_paths(Path::new("/nonexistent/aurcache/list")),
            Ok(Vec::new())
        );
    }

    /// A list that is there but cannot be read refuses the build rather than
    /// protecting nothing: that is what a policy at `0600 root` looks like to
    /// the build user. A directory in its place stands in for it, as root
    /// reads any file whatever its mode. So does a symlink to nothing, in its
    /// place or on the way to it (`/etc/aurcache -> /nowhere`).
    #[test]
    fn an_unreadable_protected_list_refuses() {
        let dir = std::env::temp_dir().join(format!(
            "aurcache-sandbox-test-unreadable-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("list")).unwrap();
        std::fs::write(dir.join("file"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("nothing"), dir.join("dangling")).unwrap();

        for unreadable in [
            dir.join("list"),
            dir.join("file/list"),
            dir.join("dangling"),
            dir.join("dangling/list"),
        ] {
            let refused = protected_paths(&unreadable).unwrap_err();
            assert!(
                refused.contains(&unreadable.display().to_string()),
                "{refused}"
            );
        }
        // A link to nothing is named as one, not as a missing file.
        for dangling in [dir.join("dangling"), dir.join("dangling/list")] {
            let refused = protected_paths(&dangling).unwrap_err();
            assert!(refused.contains("a symlink to nothing"), "{refused}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A FIFO in the list's place is refused without being read: reading one
    /// would block until something writes to it, so the build would hang.
    /// So is a device, such as `/dev/zero` behind a link, which never ends,
    /// and a file larger than any list of paths.
    #[test]
    fn a_protected_list_that_is_not_a_file_is_refused_unread() {
        let dir = std::env::temp_dir().join(format!(
            "aurcache-sandbox-test-not-a-file-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("fifo");
        let c_fifo = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_fifo` is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o644) }, 0);
        std::os::unix::fs::symlink("/dev/zero", dir.join("zero")).unwrap();
        std::fs::File::create(dir.join("large"))
            .unwrap()
            .set_len(MAX_POLICY_BYTES + 1)
            .unwrap();

        for not_a_file in [fifo, dir.join("zero")] {
            let refused = protected_paths(&not_a_file).unwrap_err();
            assert!(refused.contains("not a regular file"), "{refused}");
        }
        let refused = protected_paths(&dir.join("large")).unwrap_err();
        assert!(refused.contains("larger than"), "{refused}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
