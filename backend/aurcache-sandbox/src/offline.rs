//! The `offline` profile: run a program, and all it starts, with no network.
//!
//! ```text
//! aurcache-sandbox offline -- PROGRAM [ARGS]...
//! ```
//!
//! For installing a build's dependencies, which runs other packages'
//! `.install` scriptlets and alpm hooks as root: whatever they do, they reach
//! no other machine. Root here is not trusted with less than that, and no
//! privilege is needed to apply any of it, so it holds the same in a
//! container whose root has no `CAP_SYS_ADMIN`:
//!
//! - `no_new_privs`, so no setuid binary or file capability undoes the rest;
//! - `CAP_NET_RAW` and `CAP_NET_ADMIN` dropped from every capability set;
//! - a seccomp filter allowing only `AF_UNIX` sockets and no io_uring
//!   (`seccomp.rs`): IP of any kind, packet sockets and netlink fail with
//!   `EAFNOSUPPORT`, on any kernel;
//! - Landlock as far as the kernel goes: no TCP (ABI v4), no abstract UNIX
//!   socket or signal outside the sandbox (v6), and no connecting to a
//!   socket by path (v9). The filesystem is otherwise left alone: the
//!   install writes wherever packages go.
//!
//! The seccomp filter is the boundary and is required; on an architecture
//! it has not been written for, the profile refuses rather than running
//! with less. The Landlock layer is best effort.

use std::convert::Infallible;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr, Scope,
};

use crate::seccomp;

/// The newest Landlock ABI this knows the rights of.
const LANDLOCK_ABI: ABI = ABI::V9;

/// `linux/capability.h`.
const CAP_SETPCAP: u32 = 8;
const CAP_NET_ADMIN: u32 = 12;
const CAP_NET_RAW: u32 = 13;
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// The capabilities a program without network has no use for.
const DROPPED: [u32; 2] = [CAP_NET_ADMIN, CAP_NET_RAW];

struct Request {
    program: OsString,
    args: Vec<OsString>,
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
    let arch = seccomp::native()
        .ok_or("no seccomp filter is written for this architecture, and it is the boundary")?;
    let filter = seccomp::program(&arch);

    // SAFETY: a plain prctl; it cannot be undone, which is the point.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(format!(
            "cannot set no_new_privs: {}",
            std::io::Error::last_os_error()
        ));
    }
    drop_capabilities()?;
    layer_landlock()?;
    // Last: everything above makes system calls the filter does not concern
    // itself with, but nothing after it but the exec should run confined by
    // only part of the profile.
    seccomp::install(&filter).map_err(|e| {
        // qemu's user-mode emulation refuses every filter, so an emulated
        // worker lands here: there is no filter it could install that would
        // describe the system calls the host actually sees.
        let emulated = if e.raw_os_error() == Some(libc::EINVAL) {
            " (an emulated process cannot install one)"
        } else {
            ""
        };
        format!("cannot install the seccomp filter: {e}{emulated}")
    })?;

    let err = std::process::Command::new(&request.program)
        .args(&request.args)
        .exec();
    Err(format!(
        "cannot execute {}: {err}",
        request.program.to_string_lossy()
    ))
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
/// an unprivileged caller does not; such a caller holds neither capability
/// and, under `no_new_privs`, cannot gain one by exec, so for it the
/// bounding set is left as it is.
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
fn layer_landlock() -> Result<(), String> {
    Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessNet::from_all(LANDLOCK_ABI))
        .and_then(|r| r.handle_access(AccessFs::ResolveUnix))
        .and_then(|r| r.scope(Scope::from_all(LANDLOCK_ABI)))
        .and_then(Ruleset::create)
        .and_then(landlock::RulesetCreated::restrict_self)
        .map_err(|e| format!("cannot apply the Landlock layer: {e}"))?;
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

    /// The filter is the boundary, so every target the worker is built for
    /// must have one.
    #[test]
    fn this_target_has_a_filter() {
        assert!(seccomp::native().is_some());
    }
}
