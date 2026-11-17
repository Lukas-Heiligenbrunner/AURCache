//! What being the container's first process asks of the agent.
//!
//! The kernel delivers to PID 1 only the signals it has a handler for, so a
//! stop has to be asked for here to be heard at all. And a build is stopped
//! by ending everything: the worker that sent the signal already knows why.

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

/// Make the agent non-dumpable, so a build process cannot `ptrace` it or
/// read its memory through `/proc`. On a host with `ptrace_scope=0` a process
/// of the same uid could otherwise read the agent while the SSH window holds
/// a connection open; the key is never in the agent's memory, but this closes
/// the path regardless.
pub fn seal_memory() -> io::Result<()> {
    // SAFETY: `prctl(PR_SET_DUMPABLE, 0)` takes no pointer and has no
    // precondition.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Kill every other process and exit when told to stop.
pub fn stop_on_signal() -> io::Result<()> {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let handler = stop as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SAFETY: `stop` only makes async-signal-safe calls.
        if unsafe { libc::signal(signal, handler) } == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The handler: nothing of the build survives the agent, and no report is
/// written. Both calls are async-signal-safe.
extern "C" fn stop(_signal: libc::c_int) {
    // SAFETY: installed only once `placed` held, so `-1` is the container's
    // other processes and nothing else.
    unsafe {
        libc::kill(-1, libc::SIGKILL);
        libc::_exit(STOPPED);
    }
}
