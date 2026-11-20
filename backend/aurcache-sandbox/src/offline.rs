//! The `offline` profile: run a program, and all it starts, with no network.
//!
//! ```text
//! aurcache-sandbox offline -- PROGRAM [ARGS]...
//! ```
//!
//! For installing a build's dependencies, which runs other packages'
//! `.install` scriptlets and alpm hooks as root: whatever they do, they reach
//! no other machine. Every layer that can be applied is, in this order:
//!
//! 1. `no_new_privs`, so no setuid binary or file capability undoes the rest;
//! 2. a network namespace of its own, with nothing in it (its loopback is
//!    left down), where creating one is permitted: root with
//!    `CAP_SYS_ADMIN`, as in a chroot worker's container;
//! 3. a seccomp filter allowing only `AF_UNIX` sockets, no io_uring and no
//!    kernel keyring (`seccomp.rs`): IP of any kind, packet sockets, netlink
//!    and vsock fail with `EAFNOSUPPORT`. It needs no privilege, but an
//!    emulated process cannot install one;
//! 4. `CAP_NET_ADMIN`, `CAP_NET_RAW`, `CAP_SYS_ADMIN` and `CAP_SYS_PTRACE`
//!    dropped from every capability set, the bounding set included: without
//!    the last two, root could `setns` back into a namespace with network
//!    through any process still in one, or ptrace such a process;
//! 5. Landlock as far as the kernel goes: no TCP (ABI v4), no abstract UNIX
//!    socket or signal outside the sandbox (v6), and no connecting to a
//!    socket by path (v9). The filesystem is otherwise left alone: the
//!    install writes wherever packages go.
//!
//! Either network layer alone leaves something the other covers:
//!
//! - the namespace alone, as on an emulated worker, leaves `AF_VSOCK`, which
//!   no network namespace confines, and the kernel keyring, whose
//!   `request_key` has the host resolve names: the build script's container
//!   refuses the keyring calls, and where no filter can be installed at all,
//!   qemu's user mode implements none of them, so there only `AF_VSOCK` is
//!   left;
//! - the filter alone leaves `AF_UNIX`, and with it the abstract UNIX
//!   sockets of the network namespace the program started in, unless
//!   Landlock scopes them (ABI 6, Linux 6.12).
//!
//! With neither, the profile refuses. It says on stderr which applied.

use std::convert::Infallible;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetStatus,
    Scope,
};

use crate::seccomp;

/// The newest Landlock ABI this knows the rights of.
const LANDLOCK_ABI: ABI = ABI::V9;
const LANDLOCK_ABI_NUMBER: i64 = 9;

/// `linux/capability.h`.
const CAP_SETPCAP: u32 = 8;
const CAP_NET_ADMIN: u32 = 12;
const CAP_NET_RAW: u32 = 13;
const CAP_SYS_PTRACE: u32 = 19;
const CAP_SYS_ADMIN: u32 = 21;
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// The capabilities taken away: the two a program without network has no
/// use for, and the two that would let root leave its namespace again.
const DROPPED: [u32; 4] = [CAP_NET_ADMIN, CAP_NET_RAW, CAP_SYS_ADMIN, CAP_SYS_PTRACE];

struct Request {
    program: OsString,
    args: Vec<OsString>,
}

/// Whether one layer took, and why not.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Layer {
    Applied,
    Unavailable(String),
}

/// How much of Landlock this kernel enforced.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Landlock {
    /// The rights of this ABI, which is the kernel's or this binary's
    /// newest, whichever is older.
    Abi(i64),
    Unavailable,
}

/// What the profile managed to apply.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Layers {
    netns: Layer,
    seccomp: Layer,
    landlock: Landlock,
}

impl Layers {
    /// Refuse unless at least one of the two network layers applied.
    fn check(&self) -> Result<(), String> {
        match (&self.netns, &self.seccomp) {
            (Layer::Unavailable(netns), Layer::Unavailable(seccomp)) => Err(format!(
                "no network layer applied: no network namespace ({netns}), \
                 and no seccomp filter ({seccomp})"
            )),
            _ => Ok(()),
        }
    }

    /// The layers that applied, as the log line names them.
    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.netns == Layer::Applied {
            parts.push("netns".to_string());
        }
        if self.seccomp == Layer::Applied {
            parts.push("seccomp".to_string());
        }
        if let Landlock::Abi(abi) = self.landlock {
            parts.push(format!("landlock(abi {abi})"));
        }
        parts.join("+")
    }
}

pub fn main(args: impl Iterator<Item = OsString>) -> ExitCode {
    let request = match parse_args(args) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("aurcache-sandbox offline: {e}");
            eprintln!("usage: aurcache-sandbox offline -- PROGRAM [ARGS]...");
            return ExitCode::from(2);
        }
    };
    match run(&request) {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("aurcache-sandbox offline: refusing to run: {e}");
            ExitCode::from(1)
        }
    }
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Request, String> {
    if args.next().is_none_or(|arg| arg != "--") {
        return Err("expected `--` after `offline`".into());
    }
    let program = args.next().ok_or("missing PROGRAM after `--`")?;
    Ok(Request {
        program,
        args: args.collect(),
    })
}

/// Confine this process and exec the program; returns only on failure.
fn run(request: &Request) -> Result<Infallible, String> {
    // SAFETY: a plain prctl; it cannot be undone, which is the point.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(format!(
            "cannot set no_new_privs: {}",
            std::io::Error::last_os_error()
        ));
    }
    // The namespace before the capabilities go: creating one takes
    // CAP_SYS_ADMIN.
    let netns = enter_netns();
    let seccomp = install_seccomp();
    drop_capabilities()?;
    let landlock = layer_landlock()?;
    let layers = Layers {
        netns,
        seccomp,
        landlock,
    };
    layers.check()?;
    eprintln!("aurcache-sandbox offline: {}", layers.summary());

    let err = std::process::Command::new(&request.program)
        .args(&request.args)
        .exec();
    Err(format!(
        "cannot execute {}: {err}",
        request.program.to_string_lossy()
    ))
}

/// Leave the network namespace this was started in for a new, empty one.
fn enter_netns() -> Layer {
    // SAFETY: a plain system call on this single-threaded process.
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } == 0 {
        Layer::Applied
    } else {
        Layer::Unavailable(std::io::Error::last_os_error().to_string())
    }
}

/// The socket-family filter, where this architecture has one and this
/// process can install it.
fn install_seccomp() -> Layer {
    let Some(arch) = seccomp::native() else {
        return Layer::Unavailable("none is written for this architecture".into());
    };
    match seccomp::install(&seccomp::program(&arch, &seccomp::OFFLINE)) {
        Ok(()) => Layer::Applied,
        // qemu's user-mode emulation refuses every filter: none it could
        // install would describe the system calls the host actually sees.
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            Layer::Unavailable(format!("{e}; an emulated process cannot install one"))
        }
        Err(e) => Layer::Unavailable(e.to_string()),
    }
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Take [`DROPPED`] out of the bounding, ambient, effective, permitted and
/// inheritable sets.
///
/// Dropping from the bounding set takes `CAP_SETPCAP`, which root has and
/// an unprivileged caller does not; such a caller holds none of them and,
/// under `no_new_privs`, cannot gain one by exec, so for it the bounding set
/// is left as it is.
fn drop_capabilities() -> Result<(), String> {
    let mut header = CapHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapData::default(); 2];
    // SAFETY: version 3 takes two data structs, which `data` is.
    if unsafe { libc::syscall(libc::SYS_capget, &raw mut header, data.as_mut_ptr()) } != 0 {
        return Err(format!("capget: {}", std::io::Error::last_os_error()));
    }
    let held_setpcap = data[0].effective & (1 << CAP_SETPCAP) != 0;
    for cap in DROPPED {
        // SAFETY: plain prctls on this process's own sets.
        unsafe {
            if held_setpcap && libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) != 0 {
                return Err(format!(
                    "cannot drop capability {cap} from the bounding set: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // EINVAL only on a kernel without ambient capabilities, which
            // then has none to lower.
            libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_LOWER, cap, 0, 0);
        }
        let set = &mut data[(cap / 32) as usize];
        let bit = !(1 << (cap % 32));
        set.effective &= bit;
        set.permitted &= bit;
        set.inheritable &= bit;
    }
    // SAFETY: as for capget; lowering a set needs no privilege.
    if unsafe { libc::syscall(libc::SYS_capset, &raw mut header, data.as_ptr()) } != 0 {
        return Err(format!("capset: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// The Landlock layer, as much of it as this kernel has.
///
/// Of the filesystem rights only connecting to a socket by path is handled,
/// with no rule granting it anywhere: the install reaches no daemon through
/// the chroot, and every other right stays the permissions' business.
fn layer_landlock() -> Result<Landlock, String> {
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessNet::from_all(LANDLOCK_ABI))
        .and_then(|r| r.handle_access(AccessFs::ResolveUnix))
        .and_then(|r| r.scope(Scope::from_all(LANDLOCK_ABI)))
        .and_then(Ruleset::create)
        .and_then(landlock::RulesetCreated::restrict_self)
        .map_err(|e| format!("cannot apply the Landlock layer: {e}"))?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Ok(Landlock::Unavailable);
    }
    Ok(kernel_landlock_abi().map_or(Landlock::Unavailable, |abi| {
        Landlock::Abi(abi.min(LANDLOCK_ABI_NUMBER))
    }))
}

/// The Landlock ABI the running kernel speaks.
fn kernel_landlock_abi() -> Option<i64> {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: the version query takes no ruleset and returns a number.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    // `c_long` is 32 bits wide on armv7.
    #[allow(clippy::useless_conversion)]
    (abi > 0).then(|| i64::from(abi))
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

    fn unavailable(why: &str) -> Layer {
        Layer::Unavailable(why.to_string())
    }

    #[test]
    fn a_program_follows_the_separator() {
        let request = parse_args(args(&["--", "pacman", "-S", "--", "foo"])).unwrap();
        assert_eq!(request.program, OsString::from("pacman"));
        assert_eq!(request.args, vec!["-S", "--", "foo"]);
    }

    #[test]
    fn the_separator_and_a_program_are_required() {
        assert!(parse_args(args(&["pacman"])).is_err());
        assert!(parse_args(args(&["--"])).is_err());
        assert!(parse_args(args(&[])).is_err());
    }

    /// Every target the worker is built for has a filter.
    #[test]
    fn this_target_has_a_filter() {
        assert!(seccomp::native().is_some());
    }

    #[test]
    fn every_layer_is_named() {
        let layers = Layers {
            netns: Layer::Applied,
            seccomp: Layer::Applied,
            landlock: Landlock::Abi(9),
        };
        assert!(layers.check().is_ok());
        assert_eq!(layers.summary(), "netns+seccomp+landlock(abi 9)");
    }

    /// In a container whose root cannot unshare, the filter alone holds.
    #[test]
    fn seccomp_alone_is_enough() {
        let layers = Layers {
            netns: unavailable("Operation not permitted"),
            seccomp: Layer::Applied,
            landlock: Landlock::Abi(6),
        };
        assert!(layers.check().is_ok());
        assert_eq!(layers.summary(), "seccomp+landlock(abi 6)");
    }

    /// Emulated, the filter cannot be installed, and the namespace holds.
    #[test]
    fn a_namespace_alone_is_enough() {
        let layers = Layers {
            netns: Layer::Applied,
            seccomp: unavailable("Invalid argument; an emulated process cannot install one"),
            landlock: Landlock::Unavailable,
        };
        assert!(layers.check().is_ok());
        assert_eq!(layers.summary(), "netns");
    }

    /// Landlock is no network layer of its own: its UDP rights are newer
    /// than this binary knows, so it alone would leave UDP open.
    #[test]
    fn neither_network_layer_is_a_refusal() {
        let layers = Layers {
            netns: unavailable("Operation not permitted"),
            seccomp: unavailable("Invalid argument; an emulated process cannot install one"),
            landlock: Landlock::Abi(9),
        };
        let err = layers.check().unwrap_err();
        assert!(err.contains("Operation not permitted"), "{err}");
        assert!(err.contains("emulated"), "{err}");
    }
}
