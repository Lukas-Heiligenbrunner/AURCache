//! The sandbox's seccomp filters, one per [`Policy`]:
//!
//! - [`OFFLINE`], for the `offline` profile: no socket but a UNIX one, and no
//!   kernel keyring;
//! - [`NO_VSOCK`], for the build, `container` and `parse` profiles: every
//!   socket but an `AF_VSOCK` one.
//!
//! Classic BPF, written out by hand: the filters are a few dozen
//! instructions, and the crates that would generate them do not all know the
//! 32-bit ARM the worker also runs on.
//!
//! - `socket(2)` and `socketpair(2)` fail with `EAFNOSUPPORT` for a family
//!   the policy refuses. Under [`OFFLINE`] that is every family but
//!   `AF_UNIX`: IPv4 and IPv6 (TCP, UDP, SCTP, raw), packet sockets,
//!   netlink, vsock, and whatever a kernel adds later. Under [`NO_VSOCK`] it
//!   is `AF_VSOCK` alone, which no network namespace confines and no
//!   firewall sees: a process in a virtual machine reaches the hypervisor's
//!   vsock services through it, and the machine's own vsock listeners;
//! - `io_uring_setup(2)` fails with `ENOSYS` under both, since a ring can
//!   create sockets of any family without the `socket` system call, and
//!   programs that probe for io_uring fall back on it;
//! - under [`OFFLINE`], `add_key(2)`, `request_key(2)` and `keyctl(2)` fail
//!   with `ENOSYS`: the kernel keyring is no namespace's, and
//!   `request_key("dns_resolver", ...)` has the host resolve a name, through
//!   a helper the kernel starts outside every namespace -- a DNS query no
//!   network layer of this process sees;
//! - a system call made through another ABI than this binary's is not
//!   filtered by this binary's numbers. Under [`OFFLINE`] it is refused: the
//!   process is killed for a foreign architecture. Under [`NO_VSOCK`], which
//!   holds whole builds, the 32-bit ABI the kernel also runs (i386 on
//!   x86-64, 32-bit ARM on aarch64) is filtered by its own numbers, so a
//!   build's 32-bit glibc programs keep working, and any other ABI is
//!   killed. On x86-64 the x32 range fails with `ENOSYS` under both. The
//!   `socketcall` multiplexer passes its arguments behind a pointer the
//!   filter cannot read, so it may not create a socket at all: glibc, built
//!   for Linux 4.4 and later as Arch Linux builds it, calls `socket`
//!   directly, but a 32-bit program that creates its sockets through
//!   `socketcall` -- a Go `linux/386` binary, some i386 musl and old glibc
//!   builds -- gets `ENOSYS` for every socket, of any family. nspawn's own
//!   address-family option leaves this ABI open: see "The container" in
//!   `design/proposed/build-runner.md`.
//!
//! A filter is inherited across `fork` and `execve` and cannot be removed,
//! so everything the confined program starts is held to it. It is the
//! kernel's, not the program's: a filter installed natively also holds a
//! program qemu runs emulated, since it is qemu's own system calls the
//! kernel sees. An emulated process cannot install one, though: qemu refuses.

/// One BPF instruction, as the kernel reads it.
pub type Instruction = libc::sock_filter;

// Classic BPF opcodes (linux/bpf_common.h).
// BPF_LD | BPF_W | BPF_ABS, BPF_JMP | BPF_JEQ | BPF_K, BPF_JMP | BPF_JGE | BPF_K.
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x05 | 0x10;
const BPF_JMP_JGE_K: u16 = 0x05 | 0x30;
const BPF_RET_K: u16 = 0x06;

// Return values (linux/seccomp.h).
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_ALLOW: u32 = 0x7fff_0000;

// Offsets into `struct seccomp_data`. `args[0]` is a 64-bit field; the low
// half comes first on every little-endian target this is built for, and the
// kernel reads the address family as an `int`, so the low half is all of it.
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
const OFFSET_ARG0_LOW: u32 = 16;

/// `linux/net.h`: what `socketcall`'s first argument names to create one.
const SYS_SOCKET: u32 = 1;
const SYS_SOCKETPAIR: u32 = 8;

/// The system calls one ABI makes, as the filter sees them.
pub struct Abi {
    /// `AUDIT_ARCH_*`: what `seccomp_data.arch` holds for this ABI.
    pub audit: u32,
    pub socket: u32,
    pub socketpair: u32,
    pub io_uring_setup: u32,
    /// `add_key`, `request_key` and `keyctl`.
    pub keyring: [u32; 3],
    /// Numbers at or above this belong to another ABI sharing `audit` (x32).
    pub foreign_from: Option<u32>,
    /// A multiplexer that could create a socket without `socket` itself.
    pub socketcall: Option<u32>,
}

/// The ABIs a process of this binary's architecture may call through.
pub struct Arch {
    /// This binary's own.
    pub native: Abi,
    /// The 32-bit one the kernel may also run, for programs built for it.
    pub compat: Option<Abi>,
}

/// The socket families `socket` and `socketpair` may create.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Families {
    /// `AF_UNIX` alone.
    UnixOnly,
    /// Every family but `AF_VSOCK`.
    AllButVsock,
}

/// What becomes of the kernel keyring's calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyring {
    Refused,
    Allowed,
}

/// What becomes of a call made through [`Arch::compat`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compat {
    /// The process is killed.
    Killed,
    /// It is held to the same policy, by that ABI's numbers.
    Filtered,
}

/// What one filter refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub families: Families,
    pub keyring: Keyring,
    pub compat: Compat,
}

/// A dependency install: no network of any kind.
pub const OFFLINE: Policy = Policy {
    families: Families::UnixOnly,
    keyring: Keyring::Refused,
    compat: Compat::Killed,
};

/// A build, and the code a PKGBUILD runs outside its chroot: everything but
/// vsock.
pub const NO_VSOCK: Policy = Policy {
    families: Families::AllButVsock,
    keyring: Keyring::Allowed,
    compat: Compat::Filtered,
};

/// A system call number as the filter compares it.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn nr(number: libc::c_long) -> u32 {
    number as u32
}

/// i386, as the kernel's table numbers it (`syscall_32.tbl`).
#[cfg(target_arch = "x86_64")]
const I386: Abi = Abi {
    audit: 0x4000_0003,
    socket: 359,
    socketpair: 360,
    io_uring_setup: 425,
    keyring: [286, 287, 288],
    foreign_from: None,
    socketcall: Some(102),
};

/// 32-bit ARM, as the kernel's table numbers it (`arch/arm/tools/syscall.tbl`).
#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
const ARM_AUDIT: u32 = 0x4000_0028;

/// 32-bit ARM run by an aarch64 kernel. Only the old ABI has `socketcall`;
/// a kernel that still accepts it must not have it create sockets.
#[cfg(target_arch = "aarch64")]
const ARM: Abi = Abi {
    audit: ARM_AUDIT,
    socket: 281,
    socketpair: 288,
    io_uring_setup: 425,
    keyring: [309, 310, 311],
    foreign_from: None,
    socketcall: Some(102),
};

/// The architecture this binary was built for, or `None` where the filter
/// has not been written for it.
#[must_use]
pub fn native() -> Option<Arch> {
    #[cfg(target_arch = "x86_64")]
    return Some(Arch {
        native: Abi {
            audit: 0xc000_003e,
            socket: nr(libc::SYS_socket),
            socketpair: nr(libc::SYS_socketpair),
            io_uring_setup: nr(libc::SYS_io_uring_setup),
            keyring: [
                nr(libc::SYS_add_key),
                nr(libc::SYS_request_key),
                nr(libc::SYS_keyctl),
            ],
            foreign_from: Some(0x4000_0000),
            socketcall: None,
        },
        compat: Some(I386),
    });
    #[cfg(target_arch = "aarch64")]
    return Some(Arch {
        native: Abi {
            audit: 0xc000_00b7,
            socket: nr(libc::SYS_socket),
            socketpair: nr(libc::SYS_socketpair),
            io_uring_setup: nr(libc::SYS_io_uring_setup),
            keyring: [
                nr(libc::SYS_add_key),
                nr(libc::SYS_request_key),
                nr(libc::SYS_keyctl),
            ],
            foreign_from: None,
            socketcall: None,
        },
        compat: Some(ARM),
    });
    #[cfg(target_arch = "arm")]
    return Some(Arch {
        native: Abi {
            audit: ARM_AUDIT,
            socket: nr(libc::SYS_socket),
            socketpair: nr(libc::SYS_socketpair),
            io_uring_setup: nr(libc::SYS_io_uring_setup),
            keyring: [
                nr(libc::SYS_add_key),
                nr(libc::SYS_request_key),
                nr(libc::SYS_keyctl),
            ],
            foreign_from: None,
            // Only the old ABI has it; an EABI kernel that still accepts it
            // must not have it create sockets.
            socketcall: Some(102),
        },
        compat: None,
    });
    #[allow(unreachable_code)]
    None
}

fn load(offset: u32) -> Instruction {
    Instruction {
        code: BPF_LD_W_ABS,
        jt: 0,
        jf: 0,
        k: offset,
    }
}

fn jump(code: u16, value: u32, jt: u8, jf: u8) -> Instruction {
    Instruction {
        code,
        jt,
        jf,
        k: value,
    }
}

fn ret(value: u32) -> Instruction {
    Instruction {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k: value,
    }
}

#[allow(clippy::cast_sign_loss)]
fn errno(code: libc::c_int) -> u32 {
    RET_ERRNO | (code as u32 & 0xffff)
}

/// The filter for `arch` under `policy`.
///
/// One block per ABI it lets through, each entered when `seccomp_data.arch`
/// names that ABI and ending in a return; a call from any other ABI falls
/// through them all and kills the process.
#[must_use]
pub fn program(arch: &Arch, policy: &Policy) -> Vec<Instruction> {
    let mut p = vec![load(OFFSET_ARCH)];
    let compat = match policy.compat {
        Compat::Filtered => arch.compat.as_ref(),
        Compat::Killed => None,
    };
    for abi in std::iter::once(&arch.native).chain(compat) {
        let block = abi_block(abi, policy);
        let skip = u8::try_from(block.len()).expect("an ABI's block fits one jump");
        p.push(jump(BPF_JMP_JEQ_K, abi.audit, 0, skip));
        p.extend(block);
    }
    p.push(ret(RET_KILL_PROCESS));
    p
}

/// The decisions for one ABI's calls; every path through it returns.
fn abi_block(abi: &Abi, policy: &Policy) -> Vec<Instruction> {
    let mut p = vec![load(OFFSET_NR)];
    if let Some(from) = abi.foreign_from {
        p.extend([jump(BPF_JMP_JGE_K, from, 0, 1), ret(errno(libc::ENOSYS))]);
    }
    let keyring = match policy.keyring {
        Keyring::Refused => &abi.keyring[..],
        Keyring::Allowed => &[],
    };
    for &nr in std::iter::once(&abi.io_uring_setup).chain(keyring) {
        p.extend([jump(BPF_JMP_JEQ_K, nr, 0, 1), ret(errno(libc::ENOSYS))]);
    }
    if let Some(nr) = abi.socketcall {
        p.extend(socketcall_rule(nr, policy.families));
    }
    for nr in [abi.socket, abi.socketpair] {
        p.extend(socket_rule(nr, policy.families));
    }
    p.push(ret(RET_ALLOW));
    p
}

/// `socket` or `socketpair` (`nr`): its family decides.
///
/// Not this call: skip the four that decide it. The load replaces the number
/// in the accumulator, so every path after it returns.
fn socket_rule(nr: u32, families: Families) -> [Instruction; 5] {
    let refused = ret(errno(libc::EAFNOSUPPORT));
    let allowed = ret(RET_ALLOW);
    #[allow(clippy::cast_sign_loss)]
    let (family, on_match, otherwise) = match families {
        Families::UnixOnly => (libc::AF_UNIX as u32, allowed, refused),
        Families::AllButVsock => (libc::AF_VSOCK as u32, refused, allowed),
    };
    [
        jump(BPF_JMP_JEQ_K, nr, 0, 4),
        load(OFFSET_ARG0_LOW),
        jump(BPF_JMP_JEQ_K, family, 0, 1),
        on_match,
        otherwise,
    ]
}

/// `socketcall` (`nr`), whose family is behind a pointer: it creates no
/// socket. Under [`Families::UnixOnly`] it is refused outright. Otherwise
/// only creating one is, and what it does with an existing socket is let
/// through: 32-bit glibc still accepts connections through it.
fn socketcall_rule(nr: u32, families: Families) -> Vec<Instruction> {
    let refused = ret(errno(libc::ENOSYS));
    match families {
        Families::UnixOnly => vec![jump(BPF_JMP_JEQ_K, nr, 0, 1), refused],
        Families::AllButVsock => vec![
            jump(BPF_JMP_JEQ_K, nr, 0, 5),
            load(OFFSET_ARG0_LOW),
            jump(BPF_JMP_JEQ_K, SYS_SOCKET, 2, 0),
            jump(BPF_JMP_JEQ_K, SYS_SOCKETPAIR, 1, 0),
            ret(RET_ALLOW),
            refused,
        ],
    }
}

/// Install `program` on this thread, for good. `no_new_privs` must be set.
pub fn install(program: &[Instruction]) -> std::io::Result<()> {
    let prog = libc::sock_fprog {
        len: u16::try_from(program.len()).map_err(std::io::Error::other)?,
        filter: program.as_ptr().cast_mut(),
    };
    // SAFETY: `prog` points at `program`, which outlives the call; the kernel
    // copies the filter before returning.
    let rc = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &raw const prog,
            0,
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Why [`install_checked`] installed nothing.
#[derive(Debug)]
pub enum NotInstalled {
    /// This process can install no filter at all: qemu's user-mode
    /// emulation refuses every one, and so does a kernel without
    /// `CONFIG_SECCOMP_FILTER`.
    Unsupported(std::io::Error),
    /// The kernel takes filters, but not this one: it is malformed.
    Invalid(std::io::Error),
    /// Anything else, such as neither `no_new_privs` nor `CAP_SYS_ADMIN`.
    Other(std::io::Error),
}

/// [`install`], saying why it installed nothing.
///
/// `EINVAL` means either that no filter can be installed here or that this
/// one is malformed, and only the first may be lived with. So a filter of
/// one instruction, allowing everything, settles it: where the kernel takes
/// that one, the program was refused for itself. The allowing filter then
/// stays installed, which changes nothing.
pub fn install_checked(program: &[Instruction]) -> Result<(), NotInstalled> {
    let Err(err) = install(program) else {
        return Ok(());
    };
    if err.raw_os_error() != Some(libc::EINVAL) {
        return Err(NotInstalled::Other(err));
    }
    match install(&[ret(RET_ALLOW)]) {
        Ok(()) => Err(NotInstalled::Invalid(err)),
        Err(_) => Err(NotInstalled::Unsupported(err)),
    }
}

/// Whether `AF_VSOCK` is refused to this process from here on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Vsock {
    /// [`NO_VSOCK`] is installed.
    Refused,
    /// No filter could be installed, for the reason given: vsock is open.
    Open(String),
}

impl Vsock {
    /// What to say on stderr, which lands in the build's log on a worker:
    /// nothing when it is refused.
    #[must_use]
    pub fn warning(&self) -> Option<String> {
        match self {
            Self::Refused => None,
            Self::Open(why) => Some(format!(
                "aurcache-sandbox: WARNING: AF_VSOCK is not refused ({why}). In a \
                 virtual machine, the PKGBUILD's code can reach the hypervisor's vsock \
                 services and the machine's own vsock listeners."
            )),
        }
    }
}

/// Install [`NO_VSOCK`] on this thread, for good.
///
/// Root installs it as it is: nothing it runs loses a privilege. Anyone
/// else needs `no_new_privs` for it, and is given it. A process that cannot
/// install a filter at all, as an emulated one cannot, is told so as
/// [`Vsock::Open`]; a filter the kernel refuses for itself, and anything
/// else that goes wrong, is an error.
pub fn refuse_vsock() -> Result<Vsock, String> {
    let Some(arch) = native() else {
        return Ok(Vsock::Open(
            "no filter is written for this architecture".into(),
        ));
    };
    let filter = program(&arch, &NO_VSOCK);
    let outcome = match install_checked(&filter) {
        Err(NotInstalled::Other(e)) if e.raw_os_error() == Some(libc::EACCES) => {
            // SAFETY: a plain prctl; it cannot be undone.
            if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
                return Err(format!(
                    "cannot set no_new_privs: {}",
                    std::io::Error::last_os_error()
                ));
            }
            install_checked(&filter)
        }
        outcome => outcome,
    };
    match outcome {
        Ok(()) => Ok(Vsock::Refused),
        // qemu's user-mode emulation refuses every filter: none it could
        // install would describe the system calls the host actually sees.
        Err(NotInstalled::Unsupported(e)) => Ok(Vsock::Open(format!(
            "no seccomp filter can be installed here ({e}): qemu's user-mode \
             emulation refuses one, and so does a kernel without CONFIG_SECCOMP_FILTER"
        ))),
        Err(NotInstalled::Invalid(e)) => Err(format!(
            "the kernel refused the vsock filter itself ({e}), though it takes \
             others: the filter is malformed"
        )),
        Err(NotInstalled::Other(e)) => Err(format!("cannot install the seccomp filter: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a socket call returned in the confined child, as an exit code:
    /// 0 for success, the errno otherwise.
    fn errno_of(rc: libc::c_int) -> libc::c_int {
        if rc >= 0 {
            0
        } else {
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(255)
        }
    }

    /// Run `probe` in a child under [`OFFLINE`] and return its exit code.
    fn confined(probe: fn() -> libc::c_int) -> libc::c_int {
        exit_code(in_child(probe, Some(&OFFLINE)))
    }

    /// The same under [`NO_VSOCK`].
    fn without_vsock(probe: fn() -> libc::c_int) -> libc::c_int {
        exit_code(in_child(probe, Some(&NO_VSOCK)))
    }

    /// The same with no filter at all: the control.
    fn unfiltered(probe: fn() -> libc::c_int) -> libc::c_int {
        exit_code(in_child(probe, None))
    }

    fn exit_code(status: libc::c_int) -> libc::c_int {
        assert!(libc::WIFEXITED(status), "child died: {status:#x}");
        libc::WEXITSTATUS(status)
    }

    /// Run `probe` in a child, under `policy` if one is given, and return
    /// its wait status. A fork rather than a thread: the filter cannot be
    /// removed, and the test harness's other threads must not inherit it.
    fn in_child(probe: fn() -> libc::c_int, policy: Option<&Policy>) -> libc::c_int {
        let arch = native().expect("a filter for the test target");
        let filter = policy.map(|policy| program(&arch, policy));
        // SAFETY: the child only makes system calls and exits; it never
        // returns into the test harness or touches its locks.
        unsafe {
            let pid = libc::fork();
            assert!(pid >= 0, "fork failed");
            if pid == 0 {
                // A child the filter kills leaves no core file behind.
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
                if let Some(filter) = &filter
                    && (libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                        || install(filter).is_err())
                {
                    libc::_exit(254);
                }
                libc::_exit(probe());
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(pid, &raw mut status, 0), pid);
            status
        }
    }

    fn socket(domain: libc::c_int, kind: libc::c_int, protocol: libc::c_int) -> libc::c_int {
        // SAFETY: a plain system call; the descriptor leaks into a child that
        // exits right after.
        errno_of(unsafe { libc::socket(domain, kind, protocol) })
    }

    /// The probes themselves, unconfined: what the filter refuses works
    /// without it, so a refusal below is the filter's.
    #[test]
    fn without_the_filter_ip_sockets_work() {
        assert_eq!(
            unfiltered(|| socket(libc::AF_INET, libc::SOCK_STREAM, 0)),
            0
        );
        assert_eq!(
            unfiltered(|| socket(libc::AF_INET6, libc::SOCK_DGRAM, 0)),
            0
        );
    }

    #[test]
    fn unix_sockets_still_work() {
        assert_eq!(confined(|| socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)), 0);
        assert_eq!(confined(|| socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0)), 0);
        assert_eq!(
            confined(|| {
                let mut fds = [0; 2];
                // SAFETY: `fds` has room for the two descriptors.
                errno_of(unsafe {
                    libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr())
                })
            }),
            0
        );
    }

    #[test]
    fn ip_sockets_are_refused() {
        let refused = libc::EAFNOSUPPORT;
        assert_eq!(
            confined(|| socket(libc::AF_INET, libc::SOCK_STREAM, 0)),
            refused
        );
        assert_eq!(
            confined(|| socket(libc::AF_INET, libc::SOCK_DGRAM, 0)),
            refused
        );
        assert_eq!(
            confined(|| socket(libc::AF_INET6, libc::SOCK_STREAM, 0)),
            refused
        );
        assert_eq!(
            confined(|| socket(libc::AF_INET6, libc::SOCK_DGRAM, 0)),
            refused
        );
        assert_eq!(
            confined(|| socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP)),
            refused
        );
    }

    #[test]
    fn packet_and_netlink_sockets_are_refused() {
        let refused = libc::EAFNOSUPPORT;
        assert_eq!(
            confined(|| socket(libc::AF_PACKET, libc::SOCK_RAW, 0)),
            refused
        );
        assert_eq!(
            confined(|| socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE)),
            refused
        );
        assert_eq!(
            confined(|| {
                let mut fds = [0; 2];
                // SAFETY: `fds` has room for the two descriptors.
                errno_of(unsafe {
                    libc::socketpair(libc::AF_INET, libc::SOCK_STREAM, 0, fds.as_mut_ptr())
                })
            }),
            refused
        );
    }

    /// The family is the whole `int`: a value whose low byte is `AF_UNIX`
    /// must not pass for one.
    #[test]
    fn only_the_exact_family_passes() {
        assert_eq!(
            confined(|| socket(libc::AF_UNIX | 0x100, libc::SOCK_STREAM, 0)),
            libc::EAFNOSUPPORT
        );
    }

    fn io_uring_setup() -> libc::c_int {
        let mut params = [0u8; 120];
        // SAFETY: `params` is large enough for `io_uring_params`.
        let rc = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1, params.as_mut_ptr()) };
        #[allow(clippy::cast_possible_truncation)]
        errno_of(rc as libc::c_int)
    }

    /// Whether this kernel has io_uring at all, so that an `ENOSYS` below is
    /// the filter's. Where io_uring is disabled by sysctl the kernel answers
    /// `EPERM`, which still tells the two apart.
    fn kernel_has_io_uring() -> bool {
        let has = unfiltered(io_uring_setup) != libc::ENOSYS;
        if !has {
            eprintln!("this kernel has no io_uring; skipped");
        }
        has
    }

    #[test]
    fn io_uring_is_refused() {
        if kernel_has_io_uring() {
            assert_eq!(confined(io_uring_setup), libc::ENOSYS);
        }
    }

    /// The numbers as the kernel's tables give them
    /// (`arch/x86/entry/syscalls/syscall_64.tbl`,
    /// `include/uapi/asm-generic/unistd.h`, `arch/arm/tools/syscall.tbl`).
    #[test]
    fn the_keyring_calls_are_the_kernels() {
        #[cfg(target_arch = "x86_64")]
        let kernel = [248, 249, 250];
        #[cfg(target_arch = "aarch64")]
        let kernel = [217, 218, 219];
        #[cfg(target_arch = "arm")]
        let kernel = [309, 310, 311];
        assert_eq!(native().expect("a filter").native.keyring, kernel);
    }

    /// `keyctl(KEYCTL_GET_KEYRING_ID, KEY_SPEC_SESSION_KEYRING, 0)`: a read,
    /// which creates nothing and starts no helper on the host.
    fn session_keyring() -> libc::c_int {
        const KEYCTL_GET_KEYRING_ID: libc::c_long = 0;
        const KEY_SPEC_SESSION_KEYRING: libc::c_long = -3;
        // SAFETY: a plain system call with integer arguments.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_keyctl,
                KEYCTL_GET_KEYRING_ID,
                KEY_SPEC_SESSION_KEYRING,
                0,
            )
        };
        #[allow(clippy::cast_possible_truncation)]
        errno_of(rc as libc::c_int)
    }

    /// The kernel has a keyring to refuse, so the `ENOSYS` below is the
    /// filter's.
    #[test]
    fn without_the_filter_the_keyring_answers() {
        assert_ne!(unfiltered(session_keyring), libc::ENOSYS);
    }

    #[test]
    fn the_keyring_is_refused() {
        assert_eq!(confined(session_keyring), libc::ENOSYS);
        assert_eq!(
            confined(|| {
                // SAFETY: `add_key("user", "aurcache", "x", 1, session)`;
                // every pointer is to a static string.
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_add_key,
                        c"user".as_ptr(),
                        c"aurcache".as_ptr(),
                        c"x".as_ptr(),
                        1,
                        -3,
                    )
                };
                #[allow(clippy::cast_possible_truncation)]
                errno_of(rc as libc::c_int)
            }),
            libc::ENOSYS
        );
        assert_eq!(
            confined(|| {
                // SAFETY: `request_key("dns_resolver", "example.org", NULL,
                // 0)`, which the filter refuses before the kernel could ask
                // the host to resolve anything.
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_request_key,
                        c"dns_resolver".as_ptr(),
                        c"example.org".as_ptr(),
                        std::ptr::null::<libc::c_char>(),
                        0,
                    )
                };
                #[allow(clippy::cast_possible_truncation)]
                errno_of(rc as libc::c_int)
            }),
            libc::ENOSYS
        );
    }

    /// Everything else is untouched: a confined program is an ordinary one
    /// that cannot reach the network.
    #[test]
    fn other_calls_are_allowed() {
        assert_eq!(
            confined(|| {
                // SAFETY: plain system calls.
                unsafe {
                    let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
                    errno_of(fd.min(libc::getpid()))
                }
            }),
            0
        );
    }

    /// An x32 `socket` of `family`.
    #[cfg(target_arch = "x86_64")]
    fn x32_socket(family: libc::c_int) -> libc::c_int {
        // SAFETY: a plain system call with integer arguments.
        #[allow(clippy::cast_possible_truncation)]
        let rc =
            unsafe { libc::syscall(0x4000_0000 | libc::SYS_socket, family, libc::SOCK_STREAM, 0) }
                as libc::c_int;
        errno_of(rc)
    }

    /// Whether this kernel runs x32 calls (`CONFIG_X86_X32_ABI`). Without
    /// it, it answers `ENOSYS` itself, and the x32 tests check nothing.
    #[cfg(target_arch = "x86_64")]
    fn kernel_runs_x32() -> bool {
        let runs = unfiltered(|| x32_socket(libc::AF_INET)) != libc::ENOSYS;
        if !runs {
            eprintln!("this kernel runs no x32 system calls; skipped");
        }
        runs
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_x32_range_is_refused() {
        if kernel_runs_x32() {
            assert_eq!(confined(|| x32_socket(libc::AF_INET)), libc::ENOSYS);
        }
    }

    fn socketpair(domain: libc::c_int) -> libc::c_int {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors.
        errno_of(unsafe { libc::socketpair(domain, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) })
    }

    fn vsock() -> libc::c_int {
        socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0)
    }

    /// Whether this kernel has `AF_VSOCK` at all, so that a refusal below is
    /// the filter's. Opening one loads the vsock module where it is not.
    fn kernel_has_vsock() -> bool {
        let has = unfiltered(vsock) != libc::EAFNOSUPPORT;
        if !has {
            eprintln!("this kernel has no AF_VSOCK: the refusals are its own");
        }
        has
    }

    #[test]
    fn vsock_is_refused() {
        kernel_has_vsock();
        assert_eq!(without_vsock(vsock), libc::EAFNOSUPPORT);
        assert_eq!(
            without_vsock(|| socket(libc::AF_VSOCK, libc::SOCK_DGRAM, 0)),
            libc::EAFNOSUPPORT
        );
        assert_eq!(confined(vsock), libc::EAFNOSUPPORT);
    }

    /// vsock has no socket pairs, so the kernel's own answer is another
    /// errno: the filter's comes first.
    #[test]
    fn a_vsock_pair_is_refused_by_the_filter() {
        if kernel_has_vsock() {
            assert_ne!(
                unfiltered(|| socketpair(libc::AF_VSOCK)),
                libc::EAFNOSUPPORT
            );
        }
        assert_eq!(
            without_vsock(|| socketpair(libc::AF_VSOCK)),
            libc::EAFNOSUPPORT
        );
    }

    /// Without vsock, a build keeps every other family: IP, packet sockets
    /// and netlink are the network policy's business, not this filter's.
    #[test]
    fn without_vsock_every_other_family_works() {
        assert_eq!(
            without_vsock(|| socket(libc::AF_INET, libc::SOCK_STREAM, 0)),
            0
        );
        assert_eq!(
            without_vsock(|| socket(libc::AF_INET6, libc::SOCK_DGRAM, 0)),
            0
        );
        assert_eq!(
            without_vsock(|| socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)),
            0
        );
        assert_eq!(
            without_vsock(|| socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE)),
            0
        );
        assert_eq!(without_vsock(|| socketpair(libc::AF_UNIX)), 0);
    }

    /// A ring could create a vsock socket without `socket`.
    #[test]
    fn without_vsock_io_uring_is_refused() {
        if kernel_has_io_uring() {
            assert_eq!(without_vsock(io_uring_setup), libc::ENOSYS);
        }
    }

    #[test]
    fn without_vsock_the_keyring_answers() {
        assert_ne!(without_vsock(session_keyring), libc::ENOSYS);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn without_vsock_the_x32_range_is_refused() {
        if kernel_runs_x32() {
            assert_eq!(without_vsock(|| x32_socket(libc::AF_VSOCK)), libc::ENOSYS);
        }
    }

    /// A program the kernel refuses for itself is told apart from a kernel
    /// that takes no filter, which would be lived with.
    #[test]
    fn a_malformed_filter_is_invalid() {
        let status = in_child(
            || {
                let malformed = [Instruction {
                    code: 0xffff,
                    jt: 0,
                    jf: 0,
                    k: 0,
                }];
                // SAFETY: a plain prctl in the child.
                if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
                    return 254;
                }
                match install_checked(&malformed) {
                    Err(NotInstalled::Invalid(_)) => 0,
                    Err(NotInstalled::Unsupported(_)) => 1,
                    Err(NotInstalled::Other(_)) => 2,
                    Ok(()) => 3,
                }
            },
            None,
        );
        assert_eq!(exit_code(status), 0);
    }

    /// The i386 ABI, which a 64-bit process reaches through `int 0x80`, as a
    /// 32-bit program in a build would through its own calls.
    #[cfg(target_arch = "x86_64")]
    mod i386 {
        use super::*;

        /// An i386 system call; its result, or the negated errno.
        fn call(nr: u32, a: u32, b: u32, c: u32) -> i32 {
            let rc: u32;
            // SAFETY: `int 0x80` enters the kernel's i386 entry point, which
            // reads its arguments from these registers and returns in eax.
            // rbx is LLVM's, so the first argument goes through another
            // register and back.
            unsafe {
                std::arch::asm!(
                    "xchg {a:r}, rbx",
                    "int 0x80",
                    "xchg {a:r}, rbx",
                    a = inout(reg) u64::from(a) => _,
                    inlateout("eax") nr => rc,
                    in("ecx") b,
                    in("edx") c,
                    out("r8") _, out("r9") _, out("r10") _, out("r11") _,
                    out("r12") _, out("r13") _, out("r14") _, out("r15") _,
                );
            }
            #[allow(clippy::cast_possible_wrap)]
            let rc = rc as i32;
            rc
        }

        /// What an i386 call returned, as the probes' exit code.
        fn outcome(rc: i32) -> libc::c_int {
            if rc >= 0 { 0 } else { -rc }
        }

        #[allow(clippy::cast_sign_loss)]
        fn family(af: libc::c_int) -> u32 {
            af as u32
        }

        fn vsock() -> libc::c_int {
            outcome(call(I386.socket, family(libc::AF_VSOCK), 1, 0))
        }

        fn inet() -> libc::c_int {
            outcome(call(I386.socket, family(libc::AF_INET), 1, 0))
        }

        /// `socketcall(call, args)`, its arguments where a 32-bit pointer
        /// reaches them.
        fn socketcall(op: u32, args: [u32; 3]) -> libc::c_int {
            // SAFETY: an anonymous mapping below 4 GiB, written once and
            // left to the child's exit.
            let page = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    4096,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_32BIT,
                    -1,
                    0,
                )
            };
            if page == libc::MAP_FAILED {
                return 253;
            }
            // SAFETY: the page holds far more than three words.
            unsafe { std::ptr::copy_nonoverlapping(args.as_ptr(), page.cast(), 3) };
            #[allow(clippy::cast_possible_truncation)]
            let pointer = page as usize as u32;
            outcome(call(
                I386.socketcall.expect("i386 has socketcall"),
                op,
                pointer,
                0,
            ))
        }

        /// This kernel runs i386 calls; without that, there is nothing to
        /// test.
        fn kernel_runs_i386() -> bool {
            // getpid
            let status = in_child(|| outcome(call(20, 0, 0, 0)), None);
            let runs = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
            if !runs {
                eprintln!("this kernel runs no i386 system calls; skipped");
            }
            runs
        }

        #[test]
        fn a_vsock_socket_is_refused() {
            if !kernel_runs_i386() {
                return;
            }
            if kernel_has_vsock() {
                assert_eq!(unfiltered(vsock), 0);
            }
            assert_eq!(without_vsock(vsock), libc::EAFNOSUPPORT);
            assert_eq!(without_vsock(inet), 0);
        }

        /// Creating a socket through `socketcall` is refused whatever its
        /// family; acting on one is not.
        #[test]
        fn socketcall_creates_nothing() {
            if !kernel_runs_i386() {
                return;
            }
            assert_eq!(
                unfiltered(|| socketcall(SYS_SOCKET, [family(libc::AF_INET), 1, 0])),
                0
            );
            assert_eq!(
                without_vsock(|| socketcall(SYS_SOCKET, [family(libc::AF_VSOCK), 1, 0])),
                libc::ENOSYS
            );
            assert_eq!(
                without_vsock(|| socketcall(SYS_SOCKETPAIR, [family(libc::AF_UNIX), 1, 0])),
                libc::ENOSYS
            );
            // SYS_CONNECT on no socket: the kernel's own answer.
            assert_eq!(
                without_vsock(|| socketcall(3, [u32::MAX, 0, 0])),
                libc::EBADF
            );
        }

        /// With no parameters, so the kernel answers `EFAULT` where it has
        /// io_uring at all.
        fn io_uring_setup() -> libc::c_int {
            outcome(call(I386.io_uring_setup, 1, 0, 0))
        }

        #[test]
        fn io_uring_is_refused() {
            if !kernel_runs_i386() {
                return;
            }
            if unfiltered(io_uring_setup) == libc::ENOSYS {
                eprintln!("this kernel has no i386 io_uring; skipped");
                return;
            }
            assert_eq!(without_vsock(io_uring_setup), libc::ENOSYS);
        }

        /// The offline install has no 32-bit programs to keep working.
        #[test]
        fn offline_kills_the_process() {
            if !kernel_runs_i386() {
                return;
            }
            let status = in_child(inet, Some(&OFFLINE));
            assert!(
                libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSYS,
                "status {status:#x}"
            );
        }
    }
}
