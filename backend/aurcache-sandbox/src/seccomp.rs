//! The seccomp filter of the `offline` profile: no socket but a UNIX one.
//!
//! Classic BPF, written out by hand: the filter is a dozen instructions, and
//! the crates that would generate it do not all know the 32-bit ARM the
//! worker also runs on.
//!
//! - `socket(2)` and `socketpair(2)` succeed for `AF_UNIX` and fail with
//!   `EAFNOSUPPORT` for every other family: IPv4 and IPv6 (TCP, UDP, SCTP,
//!   raw), packet sockets, netlink, and whatever a kernel adds later;
//! - `io_uring_setup(2)` fails with `ENOSYS`, since a ring can create sockets
//!   without the `socket` system call, and programs that probe for io_uring
//!   fall back on it;
//! - a system call made through another ABI than this binary's (i386 or x32
//!   on x86-64, the old ARM `socketcall`) is not filtered by number, so it
//!   is refused: the process is killed for a foreign architecture, and the
//!   x32 range fails with `ENOSYS`.
//!
//! The filter is inherited across `fork` and `execve` and cannot be removed,
//! so everything the confined program starts is held to it.

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

/// The system calls this binary's architecture makes, as the filter sees them.
pub struct Arch {
    /// `AUDIT_ARCH_*`: what `seccomp_data.arch` holds for this ABI.
    pub audit: u32,
    pub socket: u32,
    pub socketpair: u32,
    pub io_uring_setup: u32,
    /// Numbers at or above this belong to another ABI sharing `audit` (x32).
    pub foreign_from: Option<u32>,
    /// A multiplexer that could create a socket without `socket` itself.
    pub socketcall: Option<u32>,
}

/// A system call number as the filter compares it.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn nr(number: libc::c_long) -> u32 {
    number as u32
}

/// The architecture this binary was built for, or `None` where the filter
/// has not been written for it.
#[must_use]
pub fn native() -> Option<Arch> {
    #[cfg(target_arch = "x86_64")]
    return Some(Arch {
        audit: 0xc000_003e,
        socket: nr(libc::SYS_socket),
        socketpair: nr(libc::SYS_socketpair),
        io_uring_setup: nr(libc::SYS_io_uring_setup),
        foreign_from: Some(0x4000_0000),
        socketcall: None,
    });
    #[cfg(target_arch = "aarch64")]
    return Some(Arch {
        audit: 0xc000_00b7,
        socket: nr(libc::SYS_socket),
        socketpair: nr(libc::SYS_socketpair),
        io_uring_setup: nr(libc::SYS_io_uring_setup),
        foreign_from: None,
        socketcall: None,
    });
    #[cfg(target_arch = "arm")]
    return Some(Arch {
        audit: 0x4000_0028,
        socket: nr(libc::SYS_socket),
        socketpair: nr(libc::SYS_socketpair),
        io_uring_setup: nr(libc::SYS_io_uring_setup),
        foreign_from: None,
        // Only the old ABI has it; an EABI kernel that still accepts it must
        // not have it create sockets.
        socketcall: Some(102),
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

/// The filter for `arch`.
#[must_use]
pub fn program(arch: &Arch) -> Vec<Instruction> {
    let mut p = vec![
        load(OFFSET_ARCH),
        jump(BPF_JMP_JEQ_K, arch.audit, 1, 0),
        ret(RET_KILL_PROCESS),
        load(OFFSET_NR),
    ];
    if let Some(from) = arch.foreign_from {
        p.extend([jump(BPF_JMP_JGE_K, from, 0, 1), ret(errno(libc::ENOSYS))]);
    }
    for nr in std::iter::once(arch.io_uring_setup).chain(arch.socketcall) {
        p.extend([jump(BPF_JMP_JEQ_K, nr, 0, 1), ret(errno(libc::ENOSYS))]);
    }
    for nr in [arch.socket, arch.socketpair] {
        // Not this call: skip the four that decide it. The load replaces the
        // number in the accumulator, so every path after it returns.
        p.extend([
            jump(BPF_JMP_JEQ_K, nr, 0, 4),
            load(OFFSET_ARG0_LOW),
            #[allow(clippy::cast_sign_loss)]
            jump(BPF_JMP_JEQ_K, libc::AF_UNIX as u32, 0, 1),
            ret(RET_ALLOW),
            ret(errno(libc::EAFNOSUPPORT)),
        ]);
    }
    p.push(ret(RET_ALLOW));
    p
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

    /// Run `probe` in a child with the filter installed and return its exit
    /// code. A fork rather than a thread: the filter cannot be removed, and
    /// the test harness's other threads must not inherit it.
    fn confined(probe: fn() -> libc::c_int) -> libc::c_int {
        in_child(probe, true)
    }

    fn in_child(probe: fn() -> libc::c_int, filtered: bool) -> libc::c_int {
        let filter = program(&native().expect("a filter for the test target"));
        // SAFETY: the child only makes system calls and exits; it never
        // returns into the test harness or touches its locks.
        unsafe {
            let pid = libc::fork();
            assert!(pid >= 0, "fork failed");
            if pid == 0 {
                if filtered
                    && (libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                        || install(&filter).is_err())
                {
                    libc::_exit(254);
                }
                libc::_exit(probe());
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(pid, &raw mut status, 0), pid);
            assert!(libc::WIFEXITED(status), "child died: {status:#x}");
            libc::WEXITSTATUS(status)
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
            in_child(|| socket(libc::AF_INET, libc::SOCK_STREAM, 0), false),
            0
        );
        assert_eq!(
            in_child(|| socket(libc::AF_INET6, libc::SOCK_DGRAM, 0), false),
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

    #[test]
    fn io_uring_is_refused() {
        assert_eq!(
            confined(|| {
                let mut params = [0u8; 120];
                // SAFETY: `params` is large enough for `io_uring_params`.
                let rc = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1, params.as_mut_ptr()) };
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

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_x32_range_is_refused() {
        assert_eq!(
            confined(|| {
                // SAFETY: an x32 `socket`, which this filter must refuse
                // before the kernel decides whether it supports x32 at all.
                #[allow(clippy::cast_possible_truncation)]
                let rc = unsafe {
                    libc::syscall(
                        0x4000_0000 | libc::SYS_socket,
                        libc::AF_INET,
                        libc::SOCK_STREAM,
                        0,
                    )
                } as libc::c_int;
                errno_of(rc)
            }),
            libc::ENOSYS
        );
    }
}
