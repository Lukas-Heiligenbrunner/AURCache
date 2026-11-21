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
//! - **UNIX sockets**: on a kernel whose Landlock has
//!   `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` (ABI 9), a build may connect to the
//!   pathname sockets the reads above allow and to nothing beneath a protected
//!   path -- which is where the worker's ssh-agent lives. A build offered the
//!   agent is told so by `SSH_AUTH_SOCK`, and may connect to that one socket
//!   as well. Older kernels cannot say, and leave sockets unrestricted.
//!
//! `--capabilities --agent-dir DIR` builds nothing: it says whether that
//! socket rule would hold for the sockets beneath `DIR`, for a build started
//! now as the user it runs as -- whether this kernel has the right, and the
//! list could be read, protects something and protects `DIR`. The worker asks
//! it, as the build user, before it holds a key only some packages may use.
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

/// The variable naming the ssh-agent socket a build was offered.
///
/// Set by the worker only for a build offered its SSH key, and carried to here
/// by `makechrootpkg`'s `sudo --preserve-env=GNUPGHOME,SSH_AUTH_SOCK` before any
/// PKGBUILD code runs, so a PKGBUILD cannot name one for itself.
const AGENT_SOCKET_FROM_ENV: &str = "SSH_AUTH_SOCK";

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
                     aurcache-sandbox offline -- PROGRAM [ARGS]...\n       \
                     aurcache-sandbox --capabilities --agent-dir DIR";

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

/// `--capabilities --agent-dir DIR` (the first argument already taken): print
/// [`crate::RESOLVE_UNIX`] when a build started now would be refused the
/// sockets beneath `DIR` but the one it is offered, and otherwise
/// [`crate::NO_RESOLVE_UNIX`] and why not. Runs nothing.
///
/// Asked of the same checks a build makes, as the user this runs as, so it
/// cannot say yes where that user's build would not hold to it: a policy that
/// user cannot read is one it does not enforce.
pub fn capabilities(args: impl Iterator<Item = OsString>) -> ExitCode {
    let agents = match parse_capabilities_args(args) {
        Ok(agents) => agents,
        Err(e) => {
            eprintln!("aurcache-sandbox: {e}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let sockets = agent_sockets(
        resolve_unix_supported(),
        protected_paths(Path::new(PROTECTED_PATHS_FILE)),
        &agents,
    );
    println!("{}", capability_line(&sockets));
    ExitCode::SUCCESS
}

fn parse_capabilities_args(mut args: impl Iterator<Item = OsString>) -> Result<PathBuf, String> {
    if args.next().is_none_or(|arg| arg != crate::AGENT_DIR_ARG) {
        return Err(format!(
            "expected `{}` after `{}`",
            crate::AGENT_DIR_ARG,
            crate::CAPABILITIES_ARG
        ));
    }
    let dir = args
        .next()
        .filter(|dir| !dir.is_empty())
        .ok_or_else(|| format!("missing DIR after `{}`", crate::AGENT_DIR_ARG))?;
    if let Some(extra) = args.next() {
        return Err(format!("unexpected `{}`", extra.to_string_lossy()));
    }
    Ok(PathBuf::from(dir))
}

/// Whether a build would be refused the sockets beneath the agents' directory.
#[derive(Debug, PartialEq, Eq)]
enum AgentSockets {
    /// Refused, but for the one socket a build is offered.
    Refused,
    /// This kernel's Landlock cannot restrict UNIX sockets.
    KernelCannot,
    /// The protected-path list cannot be read, or names a path under a
    /// missing directory: every build is refused.
    BadPolicy(String),
    /// There is no list, or it lists nothing: nothing is protected.
    NothingProtected,
    /// The list does not protect the agents' directory.
    AgentsUnprotected(PathBuf),
}

/// The [`AgentSockets`] for `agents`, from whether this kernel can restrict
/// UNIX sockets (`kernel_can`) and the protected-path list as it was read.
fn agent_sockets(
    kernel_can: bool,
    protected: Result<Vec<PathBuf>, String>,
    agents: &Path,
) -> AgentSockets {
    // A policy every build is refused under first: it is the gap that
    // matters most, whatever the kernel.
    let protected = match protected {
        Ok(protected) => protected,
        Err(e) => return AgentSockets::BadPolicy(e),
    };
    if let Err(e) = check_parents(&protected) {
        return AgentSockets::BadPolicy(e);
    }
    if !kernel_can {
        return AgentSockets::KernelCannot;
    }
    if protected.is_empty() {
        return AgentSockets::NothingProtected;
    }
    if !protects(&protected, agents) {
        return AgentSockets::AgentsUnprotected(agents.to_path_buf());
    }
    AgentSockets::Refused
}

/// Whether `protected` keeps builds out of `dir`: where `dir` really is, it
/// is one of its paths, where that really is, or beneath one.
///
/// Judged by real paths alone, as a rule applies to what a path resolves to:
/// `dir` spelled beneath a protected path -- through a symlink out of it, or
/// with `..` -- may be anywhere. A path that cannot be resolved protects
/// nothing, and a `dir` that cannot be is not protected.
fn protects(protected: &[PathBuf], dir: &Path) -> bool {
    let Ok(real) = std::fs::canonicalize(dir) else {
        return false;
    };
    protected
        .iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .any(|path| real.starts_with(path))
}

/// What `--capabilities` prints for `sockets`: one line, the capability or
/// its absence and why.
fn capability_line(sockets: &AgentSockets) -> String {
    let why = match sockets {
        AgentSockets::Refused => return crate::RESOLVE_UNIX.to_string(),
        AgentSockets::KernelCannot => "kernel (this kernel's Landlock cannot restrict UNIX \
                                       sockets: that takes Landlock ABI 9, Linux 7.1 or later)"
            .to_string(),
        AgentSockets::BadPolicy(e) => format!("bad policy ({e})"),
        AgentSockets::NothingProtected => format!(
            "no policy ({PROTECTED_PATHS_FILE} is missing or lists nothing, so builds are \
             not kept from any path)"
        ),
        AgentSockets::AgentsUnprotected(agents) => format!(
            "policy misses the agents ({PROTECTED_PATHS_FILE} lists neither {} nor a \
             directory above it, where it really is; add it, and create it if it is missing)",
            agents.display()
        ),
    };
    format!("{}: {why}", crate::NO_RESOLVE_UNIX)
}

/// Confine this process and exec the program; returns only on failure.
fn run(request: &Request) -> Result<Infallible, Failure> {
    let protected = protected_paths(Path::new(PROTECTED_PATHS_FILE)).map_err(Failure::BadPolicy)?;
    check_parents(&protected).map_err(Failure::BadPolicy)?;
    let read = if protected.is_empty() {
        Vec::new()
    } else {
        crate::read_grants_excluding(&protected, crate::list_dir, crate::leads)
    };
    let sockets = unix_sockets(!read.is_empty() && resolve_unix_supported(), |key| {
        std::env::var_os(key).map(PathBuf::from)
    });
    restrict(&writable_dirs(), &read, &sockets).map_err(|e| Failure::Unconfined(e.to_string()))?;

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

/// Which pathname UNIX sockets made outside the sandbox a build may connect to.
#[derive(Debug, PartialEq, Eq)]
enum UnixSockets {
    /// Any: this kernel cannot restrict them, or nothing is protected.
    Any,
    /// Those beneath the read grants, and `agent` -- the ssh-agent's socket,
    /// when the build was offered it.
    Granted { agent: Option<PathBuf> },
}

/// The socket policy for a build, from whether it can be enforced (`enforce`)
/// and what the environment the build was started with names.
fn unix_sockets(enforce: bool, lookup: impl Fn(&str) -> Option<PathBuf>) -> UnixSockets {
    if !enforce {
        return UnixSockets::Any;
    }
    UnixSockets::Granted {
        agent: lookup(AGENT_SOCKET_FROM_ENV).filter(|path| !path.as_os_str().is_empty()),
    }
}

/// Whether this kernel's Landlock can restrict connecting to pathname UNIX
/// sockets (`LANDLOCK_ACCESS_FS_RESOLVE_UNIX`, ABI 9, Linux 7.1).
///
/// Asked by creating a ruleset that requires the right, which restricts
/// nothing: it is only a file descriptor, dropped at once.
fn resolve_unix_supported() -> bool {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::ResolveUnix)
        .and_then(Ruleset::create)
        .is_ok()
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
/// Each listed path is protected at its real path too, whether or not it
/// exists yet ([`crate::with_real_paths`]).
fn protected_paths(file: &Path) -> Result<Vec<PathBuf>, String> {
    let contents = match std::fs::read_to_string(file) {
        Ok(contents) => contents,
        // Missing only when there is nothing by its name: a symlink to nothing
        // is a list that cannot be read.
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && std::fs::symlink_metadata(file).is_err() =>
        {
            return Ok(Vec::new());
        }
        Err(e) => {
            return Err(format!(
                "cannot read the protected-path list {} ({e}); refusing to build \
                 without knowing what it protects",
                file.display()
            ));
        }
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

/// Apply the Landlock policy to this process (inherited by the exec'd child).
/// `read` empty leaves reads unrestricted; `sockets` says which UNIX sockets
/// made outside it the build may connect to.
fn restrict(
    allow: &[PathBuf],
    read: &[PathBuf],
    sockets: &UnixSockets,
) -> Result<(), Box<dyn std::error::Error>> {
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
    // Connecting to a socket is granted where reading is: everywhere but the
    // protected paths, of which the ssh-agent's directory is one. Handled only
    // where the kernel has the right, so the ruleset stays fully enforced on
    // one that does not.
    let resolve = match sockets {
        UnixSockets::Any => landlock::BitFlags::<AccessFs>::empty(),
        UnixSockets::Granted { .. } => AccessFs::ResolveUnix.into(),
    };
    let handled = handled | resolve;
    let read_only = read_only | resolve;
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

    // The agent's own socket, for a build that was offered it. A socket and
    // nothing else: a variable naming a directory, or `/`, would grant every
    // socket beneath it.
    if let UnixSockets::Granted {
        agent: Some(socket),
    } = sockets
        && std::fs::metadata(socket).is_ok_and(|m| {
            use std::os::unix::fs::FileTypeExt;
            m.file_type().is_socket()
        })
    {
        ruleset = crate::grant(ruleset, socket, resolve)?;
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

    /// Where sockets can be restricted, a build is granted the agent's socket
    /// only when its environment names one; where they cannot, nothing is
    /// claimed about them.
    #[test]
    fn the_agent_socket_is_granted_only_to_a_build_offered_it() {
        let offered = lookup_from(&[(
            "SSH_AUTH_SOCK",
            "/var/lib/aurcache-worker/agent/default/agent.sock",
        )]);
        assert_eq!(
            unix_sockets(true, &offered),
            UnixSockets::Granted {
                agent: Some(PathBuf::from(
                    "/var/lib/aurcache-worker/agent/default/agent.sock"
                ))
            }
        );
        assert_eq!(
            unix_sockets(true, lookup_from(&[])),
            UnixSockets::Granted { agent: None }
        );
        assert_eq!(
            unix_sockets(true, lookup_from(&[("SSH_AUTH_SOCK", "")])),
            UnixSockets::Granted { agent: None }
        );
        assert_eq!(unix_sockets(false, offered), UnixSockets::Any);
    }

    /// The shipped policy protects the agent's directory, which is what keeps
    /// a build that was not offered the key from its socket.
    #[test]
    fn the_shipped_policy_protects_the_agent() {
        let policy = include_str!("../../../packaging/sandbox-protected");
        assert!(
            policy
                .lines()
                .any(|line| line.trim() == "/var/lib/aurcache-worker/agent"),
            "packaging/sandbox-protected must list the agent's directory"
        );
    }

    /// The question takes the agents' directory, and nothing else.
    #[test]
    fn capabilities_take_the_agents_directory() {
        assert_eq!(
            parse_capabilities_args(args(&["--agent-dir", "/var/lib/aurcache-worker/agent"])),
            Ok(PathBuf::from("/var/lib/aurcache-worker/agent"))
        );
        for list in [
            &[][..],
            &["--agent-dir"],
            &["--agent-dir", ""],
            &["/var/lib/aurcache-worker/agent"],
            &["--agent-dir", "/a", "/b"],
        ] {
            assert!(parse_capabilities_args(args(list)).is_err(), "{list:?}");
        }
    }

    /// The socket rule is claimed only where a build started now would hold
    /// to it: a kernel that has the right, and a list that could be read,
    /// protects something, names no path under a missing directory, and
    /// protects the agents' directory.
    #[test]
    fn resolve_unix_is_claimed_only_where_a_build_would_enforce_it() {
        let dir = std::env::temp_dir().join(format!(
            "aurcache-sandbox-test-capabilities-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("worker/agent")).unwrap();
        let agents = dir.join("worker/agent");
        let policy = |paths: &[&str]| -> Result<Vec<PathBuf>, String> {
            Ok(paths.iter().map(|path| dir.join(path)).collect())
        };

        for protecting in [
            &["worker/agent"][..],
            &["worker"],
            &["worker/ssh", "worker/agent"],
        ] {
            assert_eq!(
                agent_sockets(true, policy(protecting), &agents),
                AgentSockets::Refused,
                "{protecting:?}"
            );
        }
        assert_eq!(
            agent_sockets(false, policy(&["worker/agent"]), &agents),
            AgentSockets::KernelCannot
        );
        for kernel_can in [true, false] {
            assert_eq!(
                agent_sockets(kernel_can, Err("cannot read it".into()), &agents),
                AgentSockets::BadPolicy("cannot read it".into())
            );
        }
        assert_eq!(
            agent_sockets(true, policy(&[]), &agents),
            AgentSockets::NothingProtected
        );
        assert!(matches!(
            agent_sockets(true, policy(&["missing/agent"]), &agents),
            AgentSockets::BadPolicy(_)
        ));
        for missing in [
            &["worker/ssh"][..],
            &["worker/agent/default"],
            &["worker/agen"],
        ] {
            assert_eq!(
                agent_sockets(true, policy(missing), &agents),
                AgentSockets::AgentsUnprotected(agents.clone()),
                "{missing:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The agents' directory reached through a symlink is protected where it
    /// really is, as the read walk protects a listed path at its real path.
    #[test]
    fn the_agents_directory_is_judged_where_it_really_is() {
        let dir = std::env::temp_dir().join(format!(
            "aurcache-sandbox-test-agents-link-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real/agent")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let real = std::fs::canonicalize(dir.join("real/agent")).unwrap();

        assert!(protects(
            std::slice::from_ref(&real),
            &dir.join("link/agent")
        ));
        assert!(!protects(&[dir.join("real/ssh")], &dir.join("link/agent")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Spelled beneath a protected path, the agents' directory is not
    /// protected where it really is elsewhere: a symlink out of a listed
    /// directory, or `..` out of one. Nor is one that does not exist.
    #[test]
    fn the_agents_directory_is_not_judged_by_its_spelling() {
        let dir = std::env::temp_dir().join(format!(
            "aurcache-sandbox-test-agents-spelling-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("w/agent")).unwrap();
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("w/agent2")).unwrap();

        assert!(protects(&[dir.join("w")], &dir.join("w/agent")));
        assert!(!protects(&[dir.join("w")], &dir.join("w/agent2")));
        assert!(!protects(
            &[dir.join("w/agent")],
            &dir.join("w/agent/../../elsewhere")
        ));
        assert!(!protects(&[dir.join("w")], &dir.join("w/missing")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One line: the capability, or its absence and why, which the worker
    /// logs as its reason.
    #[test]
    fn the_answer_is_one_line_naming_the_reason() {
        assert_eq!(capability_line(&AgentSockets::Refused), "resolve-unix");
        for (sockets, reason) in [
            (AgentSockets::KernelCannot, "kernel"),
            (
                AgentSockets::BadPolicy("Permission denied".into()),
                "bad policy",
            ),
            (AgentSockets::NothingProtected, "no policy"),
            (
                AgentSockets::AgentsUnprotected(PathBuf::from("/srv/agent")),
                "policy misses the agents",
            ),
        ] {
            let line = capability_line(&sockets);
            assert!(
                line.starts_with(&format!("no-resolve-unix: {reason} (")),
                "{line}"
            );
            assert!(!line.contains('\n'), "{line}");
        }
        assert!(
            capability_line(&AgentSockets::AgentsUnprotected(PathBuf::from(
                "/srv/agent"
            )))
            .contains("/srv/agent")
        );
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

    /// A listed path behind a symlink is protected where it really is too.
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
        assert_eq!(protected[0], dir.join("secret/key"));
        assert!(
            protected.contains(&std::fs::canonicalize(stamped.join("key")).unwrap()),
            "{protected:?}"
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
                dir.join("alias/identity"),
                std::fs::canonicalize(dir.join("real"))
                    .unwrap()
                    .join("identity"),
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
    /// reads any file whatever its mode.
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
        ] {
            let refused = protected_paths(&unreadable).unwrap_err();
            assert!(
                refused.contains(&unreadable.display().to_string()),
                "{refused}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
