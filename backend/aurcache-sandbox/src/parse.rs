//! The `parse` profile: source a PKGBUILD without trusting it.
//!
//! ```text
//! aurcache-sandbox parse --pkgbuild FILE [--private PATH]... [--network] -- PROGRAM [ARGS]...
//! ```
//!
//! `PROGRAM` is run with one more argument, a path from which it can read
//! `FILE`. What it may do is the whole interface; how that is enforced is
//! decided here and nowhere else:
//!
//! - read `FILE`, and nothing under a `--private` path;
//! - write nothing at all;
//! - signal or inspect no process outside itself;
//! - see no environment beyond `PATH`, `HOME` and `LANG`;
//! - connect over TCP only with `--network` -- where the kernel can deny it.
//!
//! # How
//!
//! **Run as root** (the container images, so every Docker install): the parse
//! runs as `aurcache-parse`, and ordinary permissions do the confining on any
//! kernel. Another uid cannot signal the server, read its `/proc/PID/environ`,
//! or open the root-owned files and sockets it uses -- the Docker socket among
//! them, since the parse user keeps no supplementary groups. Every `--private`
//! path must therefore be closed to other users; that is checked before every
//! parse, and one that is not refuses the parse, naming it. The parse cannot
//! reach `FILE` through a directory the server owns, so `FILE` is copied into
//! a memfd the parse user owns, and the program reads `/dev/fd/N`.
//!
//! Landlock is layered on top as far as the kernel goes: no writes (ABI v1),
//! no TCP unless `--network` (v4), no signals or abstract sockets across the
//! sandbox (v6). Best effort, because the uid already holds every line the
//! profile draws except TCP -- which is hardening rather than a boundary, as
//! a parse has no secrets to send and no credentials to use.
//!
//! **Run unprivileged** (a native install, whose server runs as `aurcache`):
//! Landlock is all there is, so it must hold everything -- reads, writes,
//! `truncate`, TCP and signals -- and needs ABI v6, Linux 6.12. Older is a
//! refusal, never an unconfined parse.
//!
//! `AURCACHE_SANDBOX_DISABLE=landlock` drops the Landlock layer, so the
//! permissions alone can be tested on a host that has it.

use std::ffi::{CString, OsString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope,
};

/// The user a parse runs as when the sandbox runs as root.
const PARSE_USER: &str = "aurcache-parse";

/// The environment a parse gets, and nothing else: the server's holds the
/// database password and the OAuth secret.
const ENVIRONMENT: [(&str, &str); 3] = [
    ("PATH", "/usr/local/bin:/usr/bin:/bin"),
    ("HOME", "/nonexistent"),
    ("LANG", "C.UTF-8"),
];

/// A PKGBUILD larger than this is not one; refuse it rather than copy it.
const MAX_PKGBUILD_BYTES: u64 = 4 << 20;

/// The newest Landlock ABI this knows the rights of.
const LANDLOCK_ABI: ABI = ABI::V6;

struct Request {
    pkgbuild: PathBuf,
    private: Vec<PathBuf>,
    network: bool,
    program: OsString,
    args: Vec<OsString>,
}

/// Who the parse runs as when it is not the caller.
#[derive(Clone, Copy)]
struct ParseUser {
    uid: libc::uid_t,
    gid: libc::gid_t,
}

pub fn main(args: impl Iterator<Item = OsString>) -> ExitCode {
    let request = match parse_args(args) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("aurcache-sandbox parse: {e}");
            eprintln!(
                "usage: aurcache-sandbox parse --pkgbuild FILE [--private PATH]... [--network] \
                 -- PROGRAM [ARGS]..."
            );
            return ExitCode::from(2);
        }
    };
    let landlock_disabled =
        std::env::var_os("AURCACHE_SANDBOX_DISABLE").is_some_and(|v| v == "landlock");

    match run(&request, landlock_disabled) {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("aurcache-sandbox parse: refusing to parse: {e}");
            ExitCode::from(1)
        }
    }
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Request, String> {
    let mut pkgbuild = None;
    let mut private = Vec::new();
    let mut network = false;
    let absolute = |flag: &str, value: Option<OsString>| -> Result<PathBuf, String> {
        let value = PathBuf::from(value.ok_or_else(|| format!("{flag} needs a path"))?);
        // Relative paths would resolve against wherever this was started,
        // which the caller did not necessarily mean.
        if value.is_absolute() {
            Ok(value)
        } else {
            Err(format!("{flag} must be absolute: {}", value.display()))
        }
    };
    loop {
        let Some(arg) = args.next() else {
            return Err("missing `-- PROGRAM`".into());
        };
        match arg.to_str() {
            Some("--pkgbuild") => pkgbuild = Some(absolute("--pkgbuild", args.next())?),
            Some("--private") => private.push(absolute("--private", args.next())?),
            Some("--network") => network = true,
            Some("--") => break,
            _ => return Err(format!("unexpected argument {}", arg.to_string_lossy())),
        }
    }
    let pkgbuild = pkgbuild.ok_or("--pkgbuild is required")?;
    let program = args.next().ok_or("missing PROGRAM after `--`")?;
    Ok(Request {
        pkgbuild,
        private,
        network,
        program,
        args: args.collect(),
    })
}

/// Confine this process and exec the program; returns only on failure.
fn run(request: &Request, landlock_disabled: bool) -> Result<std::convert::Infallible, String> {
    let (Some(dir), Some(file)) = (request.pkgbuild.parent(), request.pkgbuild.file_name()) else {
        return Err(format!(
            "{} does not name a file",
            request.pkgbuild.display()
        ));
    };

    // SAFETY: no arguments, cannot fail.
    let as_root = unsafe { libc::geteuid() } == 0;

    // Where the program reads the PKGBUILD from. The memfd is kept alive
    // until the exec, which inherits it.
    let (path_arg, _memfd) = if as_root {
        let user = parse_user(&request.private)?;
        let memfd = copy_to_memfd(&request.pkgbuild, user)?;
        // The server's directories are closed to the parse user; start it
        // somewhere it can stand.
        std::env::set_current_dir("/").map_err(|e| format!("cannot enter /: {e}"))?;
        drop_to(user)?;
        if !landlock_disabled {
            layer_landlock(request.network)?;
        }
        (
            OsString::from(format!("/dev/fd/{}", memfd.as_raw_fd())),
            Some(memfd),
        )
    } else {
        if landlock_disabled {
            return Err(
                "Landlock is disabled (AURCACHE_SANDBOX_DISABLE), and is all that confines an \
                 unprivileged parse"
                    .into(),
            );
        }
        restrict_with_landlock(dir, &request.private, request.network)?;
        std::env::set_current_dir(dir)
            .map_err(|e| format!("cannot enter {}: {e}", dir.display()))?;
        (OsString::from(file), None)
    };

    let err = std::process::Command::new(&request.program)
        .args(&request.args)
        .arg(path_arg)
        .env_clear()
        .envs(ENVIRONMENT)
        .exec();
    Err(format!(
        "cannot execute {}: {err}",
        request.program.to_string_lossy()
    ))
}

/// Landlock as an extra layer under the parse user: whatever this kernel
/// supports of "no writes, no TCP, nothing across the sandbox", and nothing
/// required. Reads are left to the permissions: the parse reads its PKGBUILD
/// through `/dev/fd`, which a read policy would have to make an exception for.
fn layer_landlock(network: bool) -> Result<(), String> {
    let ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_write(LANDLOCK_ABI))
        .and_then(|r| {
            if network {
                Ok(r)
            } else {
                r.handle_access(AccessNet::from_all(LANDLOCK_ABI))
            }
        })
        .and_then(|r| r.scope(Scope::from_all(LANDLOCK_ABI)))
        .and_then(Ruleset::create)
        .map_err(|e| format!("cannot build the Landlock layer: {e}"))?;
    let ruleset = match PathFd::new("/dev/null") {
        Ok(fd) => ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::WriteFile))
            .map_err(|e| e.to_string())?,
        Err(_) => ruleset,
    };
    // A kernel without Landlock reports `NotEnforced`, which is fine here.
    ruleset.restrict_self().map_err(|e| e.to_string())?;
    Ok(())
}

/// Landlock as the whole confinement, for a parse that cannot change user:
/// everything outside the private paths is readable, the PKGBUILD's
/// directory is readable even when it lies inside one, nothing is writable
/// but `/dev/null`, TCP is denied unless allowed, and nothing outside the
/// sandbox can be signalled. All of it or a refusal.
fn restrict_with_landlock(dir: &Path, private: &[PathBuf], network: bool) -> Result<(), String> {
    let private: Vec<PathBuf> = private
        .iter()
        .map(|path| shallowest_missing(path, Path::exists))
        .collect();

    // v3 is where `truncate` can be denied -- below it the parse, running as
    // the server's own uid, could empty the database by path -- and v6 where
    // signals can be. Required, not best effort: nothing else stands behind
    // this.
    let read = AccessFs::from_read(ABI::V3);
    let created = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V3))
        .and_then(|r| {
            if network {
                Ok(r)
            } else {
                r.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)
            }
        })
        .and_then(|r| r.scope(Scope::Signal | Scope::AbstractUnixSocket))
        .and_then(Ruleset::create)
        .map_err(|e| {
            format!(
                "an unprivileged parse needs Landlock and Linux 6.12 or newer, or the server \
                 must run as root ({e})"
            )
        })?;

    let mut grants = crate::read_grants_excluding(&private, crate::list_dir);
    grants.push(dir.to_path_buf());

    let mut ruleset = created;
    for path in &grants {
        ruleset = crate::grant(ruleset, path, read).map_err(|e| e.to_string())?;
    }
    if let Ok(fd) = PathFd::new("/dev/null") {
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                fd,
                AccessFs::ReadFile | AccessFs::WriteFile,
            ))
            .map_err(|e| e.to_string())?;
    }
    let status = ruleset.restrict_self().map_err(|e| e.to_string())?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => Ok(()),
        other => Err(format!("Landlock not fully enforced ({other:?})")),
    }
}

/// What to keep unreadable so that `path` is, whether or not it exists yet.
///
/// The walk that turns exclusions into grants lists each directory on the way
/// to an excluded path, so a path whose parent is missing would never be
/// reached -- and the parent, granted wholesale once created, would expose
/// it. Excluding the first missing component instead covers everything that
/// may later appear beneath it: never less than asked, sometimes more. A
/// server that has not yet created `./data` for its CA is the ordinary case.
fn shallowest_missing(path: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let mut ancestors: Vec<&Path> = path.ancestors().collect();
    ancestors.reverse();
    ancestors
        .into_iter()
        .find(|ancestor| !exists(ancestor))
        .unwrap_or(path)
        .to_path_buf()
}

/// The parse user, once the private paths are confirmed closed to it.
fn parse_user(private: &[PathBuf]) -> Result<ParseUser, String> {
    let name = CString::new(PARSE_USER).expect("no interior NUL");
    // SAFETY: `getpwnam` returns a pointer into static storage or null; this
    // process is single-threaded and copies the two fields at once.
    let (uid, gid) = unsafe {
        let entry = libc::getpwnam(name.as_ptr());
        if entry.is_null() {
            return Err(format!(
                "no user named {PARSE_USER}; the server image creates it"
            ));
        }
        ((*entry).pw_uid, (*entry).pw_gid)
    };
    if uid == 0 {
        return Err(format!("{PARSE_USER} is root"));
    }
    for path in private {
        check_closed(path, ParseUser { uid, gid })?;
    }
    Ok(ParseUser { uid, gid })
}

/// Refuse a private path the parse user could read or write.
///
/// Closed to other users is not enough when the parse user *is* the owner.
/// That is not far-fetched: NFS with `root_squash` writes root's files as
/// `nobody`, and a user-namespaced runtime shows unmapped owners as the same
/// overflow id -- which is also why the parse user is a dedicated one rather
/// than `nobody`.
///
/// A path that does not exist protects nothing and needs nothing: a server
/// that has not yet written its logs, `/etc/aurcache` in a container.
fn check_closed(path: &Path, user: ParseUser) -> Result<(), String> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Ok(());
    };
    if metadata.uid() == user.uid {
        return Err(format!(
            "{} is owned by uid {}, which is {PARSE_USER}'s: a PKGBUILD parsed as \
             {PARSE_USER} could reach it",
            path.display(),
            user.uid
        ));
    }
    let mode = metadata.mode() & 0o777;
    let open_to_group = metadata.gid() == user.gid && mode & 0o070 != 0;
    if mode & 0o007 != 0 || open_to_group {
        return Err(format!(
            "{} is mode {mode:03o}; it must be closed to other users (chmod o-rwx)",
            path.display()
        ));
    }
    Ok(())
}

/// Copy the PKGBUILD into an anonymous file owned by the parse user.
///
/// Read as root, so the path must not lead anywhere through a symlink: a
/// checkout could otherwise make `PKGBUILD` a link to the database and have
/// it copied to the parse. Nothing the PKGBUILD's author controls runs
/// before this, so nothing can change the tree between the check and the
/// open.
fn copy_to_memfd(pkgbuild: &Path, user: ParseUser) -> Result<OwnedFd, String> {
    let real = std::fs::canonicalize(pkgbuild)
        .map_err(|e| format!("cannot resolve {}: {e}", pkgbuild.display()))?;
    if real != pkgbuild {
        return Err(format!(
            "{} resolves through a symlink to {}; refusing to read it as root",
            pkgbuild.display(),
            real.display()
        ));
    }
    let mut source = std::fs::File::open(pkgbuild)
        .map_err(|e| format!("cannot open {}: {e}", pkgbuild.display()))?;
    let metadata = source.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_PKGBUILD_BYTES {
        return Err(format!(
            "{} is not a regular file under {MAX_PKGBUILD_BYTES} bytes",
            pkgbuild.display()
        ));
    }

    let name = CString::new("PKGBUILD").expect("no interior NUL");
    // SAFETY: a fresh fd, owned from here on. No MFD_CLOEXEC: the program
    // reads it after the exec.
    let memfd = unsafe {
        let fd = libc::memfd_create(name.as_ptr(), 0);
        if fd < 0 {
            return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
        }
        OwnedFd::from_raw_fd(fd)
    };
    let mut dest = std::fs::File::from(memfd);
    std::io::copy(&mut source, &mut dest).map_err(|e| format!("copying the PKGBUILD: {e}"))?;
    let memfd = OwnedFd::from(dest);
    // SAFETY: plain syscalls on an fd this function owns. The parse user
    // reopens it through /dev/fd, which checks the file's own owner and mode;
    // 0400 so it can read and not rewrite what it is given.
    unsafe {
        if libc::fchown(memfd.as_raw_fd(), user.uid, user.gid) != 0
            || libc::fchmod(memfd.as_raw_fd(), 0o400) != 0
            || libc::lseek(memfd.as_raw_fd(), 0, libc::SEEK_SET) != 0
        {
            return Err(format!(
                "preparing the PKGBUILD copy: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(memfd)
}

/// Become the parse user for good: no supplementary groups, and real,
/// effective and saved ids all changed, so nothing can switch back.
fn drop_to(ParseUser { uid, gid }: ParseUser) -> Result<(), String> {
    // SAFETY: plain syscalls; checked below as well as by return value.
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0
            || libc::setresgid(gid, gid, gid) != 0
            || libc::setresuid(uid, uid, uid) != 0
        {
            return Err(format!(
                "cannot switch to {PARSE_USER}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if libc::geteuid() != uid || libc::getuid() != uid || libc::getegid() != gid {
            return Err(format!("still not {PARSE_USER} after switching"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn a_full_request_parses() {
        let request = parse_args(args(&[
            "--pkgbuild",
            "/src/demo/PKGBUILD",
            "--private",
            "/app/db",
            "--private",
            "/etc/aurcache",
            "--network",
            "--",
            "/usr/bin/bridge",
            "-x",
        ]))
        .unwrap();
        assert_eq!(request.pkgbuild, PathBuf::from("/src/demo/PKGBUILD"));
        assert_eq!(
            request.private,
            vec![PathBuf::from("/app/db"), PathBuf::from("/etc/aurcache")]
        );
        assert!(request.network);
        assert_eq!(request.program, OsString::from("/usr/bin/bridge"));
        assert_eq!(request.args, vec![OsString::from("-x")]);
    }

    #[test]
    fn relative_paths_are_refused() {
        assert!(parse_args(args(&["--pkgbuild", "PKGBUILD", "--", "bridge"])).is_err());
        assert!(
            parse_args(args(&[
                "--pkgbuild",
                "/x/PKGBUILD",
                "--private",
                "db",
                "--",
                "bridge"
            ]))
            .is_err()
        );
    }

    #[test]
    fn the_pkgbuild_and_the_program_are_required() {
        assert!(parse_args(args(&["--", "bridge"])).is_err());
        assert!(parse_args(args(&["--pkgbuild", "/x/PKGBUILD", "--"])).is_err());
        assert!(parse_args(args(&["--pkgbuild", "/x/PKGBUILD"])).is_err());
    }

    #[test]
    fn a_private_path_open_to_others_is_refused() {
        let dir =
            std::env::temp_dir().join(format!("aurcache-sandbox-closed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Ids nobody has, so only the "other" bits are in play.
        let stranger = ParseUser {
            uid: libc::uid_t::MAX - 1,
            gid: libc::gid_t::MAX - 1,
        };
        let err = check_closed(&dir, stranger).unwrap_err();
        assert!(err.contains("755"), "{err}");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(check_closed(&dir, stranger).is_ok());

        // Closed, but owned by the parse user: what NFS root squashing does.
        // SAFETY: no arguments, cannot fail.
        let me = ParseUser {
            uid: unsafe { libc::geteuid() },
            gid: libc::gid_t::MAX - 1,
        };
        let err = check_closed(&dir, me).unwrap_err();
        assert!(err.contains("owned by"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_private_path_not_yet_created_is_covered_from_its_first_missing_part() {
        let exists = |p: &Path| ["/", "/app"].contains(&p.to_str().unwrap());
        assert_eq!(
            shallowest_missing(Path::new("/app/data/ca"), exists),
            PathBuf::from("/app/data")
        );
        let all = |_: &Path| true;
        assert_eq!(
            shallowest_missing(Path::new("/app/data/ca"), all),
            PathBuf::from("/app/data/ca")
        );
    }

    #[test]
    fn a_missing_private_path_needs_nothing() {
        let root = ParseUser { uid: 0, gid: 0 };
        assert!(check_closed(Path::new("/nonexistent/aurcache/db"), root).is_ok());
    }
}
