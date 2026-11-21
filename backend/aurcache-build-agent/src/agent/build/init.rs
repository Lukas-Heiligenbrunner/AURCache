//! What being the container's first process asks of the agent.
//!
//! The kernel delivers to PID 1 only the signals it has a handler for, so a
//! stop has to be asked for here to be heard at all. Once there is one, the
//! container's own processes can send it too, so the handler stops only for
//! a signal from outside the container. And a build is stopped by ending
//! everything: the worker that sent the signal already knows why.

use std::fmt;
use std::io;

/// The status the agent ends with when it was told to stop, as a shell
/// reports a process ended by `SIGTERM`.
const STOPPED: libc::c_int = 128 + libc::SIGTERM;

/// Why the agent will not run a build where it was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Misplaced {
    /// It is not the first process of its container. Ending a build kills
    /// every process the agent can signal, which only PID 1 can do safely.
    NotInit,
    /// It is not root, and so cannot do the root step.
    NotRoot,
}

impl fmt::Display for Misplaced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInit => write!(
                f,
                "`build` must be the container's command: it is process {}, not 1",
                std::process::id()
            ),
            Self::NotRoot => write!(f, "`build` must be started as root"),
        }
    }
}

/// Check that this process is root and the first of its PID namespace.
pub fn placed() -> Result<(), Misplaced> {
    if std::process::id() != 1 {
        return Err(Misplaced::NotInit);
    }
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(Misplaced::NotRoot);
    }
    Ok(())
}

/// Kill every other process and exit when told to stop, from outside the
/// container.
///
/// Root in the container (a dependency's install scriptlet) can signal this
/// process too; a stop it asked for would end the build without a report,
/// at a moment it chose. And this process is made one that root in the
/// container cannot trace or read the memory of, which would let it do the
/// same another way: that takes `CAP_SYS_PTRACE`, which no build container
/// has.
pub fn stop_on_signal() -> io::Result<()> {
    // SAFETY: `prctl(PR_SET_DUMPABLE)` has no memory preconditions.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: a zeroed `sigaction` is a valid one with an empty mask,
        // and every field that matters is set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = stop as Handler as libc::sighandler_t;
        // As `signal` installs one: interrupted calls resume. With the
        // sender's details, which is what the handler goes by.
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        // SAFETY: `stop` only makes async-signal-safe calls, and `action`
        // lives across the call.
        if unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

type Handler = extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void);

/// The handler: for a stop from outside, nothing of the build survives the
/// agent, and no report is written. Every call is async-signal-safe.
extern "C" fn stop(_signal: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // SAFETY: the kernel hands an `SA_SIGINFO` handler a valid `siginfo_t`,
    // and `si_pid` is set for the two codes `sender` honours.
    let (code, pid) = unsafe { ((*info).si_code, (*info).si_pid()) };
    if sender(code, pid) == Sender::Inside {
        return;
    }
    // SAFETY: installed only once `placed` held, so `-1` is the container's
    // other processes and nothing else.
    unsafe {
        libc::kill(-1, libc::SIGKILL);
        libc::_exit(STOPPED);
    }
}

/// Where a signal came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sender {
    /// A process this PID namespace does not see: the engine, the runtime,
    /// whoever stops the container.
    Outside,
    /// A process of the container's, or one whose origin cannot be told.
    Inside,
}

/// Who sent a signal, from its `si_code` and `si_pid`.
///
/// A signal from a process in an ancestor PID namespace reaches this one
/// with `si_pid` 0, the kernel's way of saying the sender has no number
/// here. That is believed only for `kill` (`SI_USER`) and `tgkill`
/// (`SI_TKILL`), where the kernel itself writes `si_pid`: it does not let
/// a process send another a signal with either code from
/// `rt_sigqueueinfo`, and with any other code (`SI_QUEUE`) the sender writes
/// `si_pid` itself and can make it 0.
const fn sender(code: libc::c_int, pid: libc::pid_t) -> Sender {
    if (code == libc::SI_USER || code == libc::SI_TKILL) && pid == 0 {
        Sender::Outside
    } else {
        Sender::Inside
    }
}

#[cfg(test)]
mod tests {
    use super::{Sender, sender};

    #[test]
    fn only_a_kill_from_outside_the_namespace_is_from_outside() {
        assert_eq!(sender(libc::SI_USER, 0), Sender::Outside);
        assert_eq!(sender(libc::SI_TKILL, 0), Sender::Outside);
        // `kill -TERM 1` from inside the container.
        assert_eq!(sender(libc::SI_USER, 42), Sender::Inside);
        assert_eq!(sender(libc::SI_TKILL, 42), Sender::Inside);
        // `sigqueue`, whose sender says who it is, even 0.
        assert_eq!(sender(libc::SI_QUEUE, 0), Sender::Inside);
        assert_eq!(sender(libc::SI_QUEUE, 42), Sender::Inside);
        assert_eq!(sender(libc::SI_KERNEL, 0), Sender::Inside);
    }
}
