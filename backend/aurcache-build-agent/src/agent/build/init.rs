//! What being the container's first process asks of the agent.
//!
//! The kernel delivers to PID 1 no signal that it has no handler for, except
//! `SIGKILL` from outside its PID namespace, and the agent keeps none. Root
//! in the container can end the build early, as interrupted, but cannot
//! forge its result (see the threat model in
//! `design/in-progress/container-workers.md`). The worker stops a build by
//! killing the container whole.

use std::io;

/// Check that this process is root and the first of its PID namespace:
/// ending a build kills every process the agent can signal, which only PID 1
/// can do safely.
pub fn placed() -> Result<(), String> {
    if std::process::id() != 1 {
        return Err(format!(
            "`build` must be the container's command: it is process {}, not 1",
            std::process::id()
        ));
    }
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err("`build` must be started as root".to_string());
    }
    Ok(())
}

/// Drop the handlers that the standard library installs for `SIGSEGV` and
/// `SIGBUS`, which would let root in the container signal the agent. A real
/// fault still kills it.
pub fn no_fault_handlers() {
    // SAFETY: restoring the default disposition has no preconditions.
    unsafe {
        libc::signal(libc::SIGSEGV, libc::SIG_DFL);
        libc::signal(libc::SIGBUS, libc::SIG_DFL);
    }
}

/// Keep root in the container, the same user, from tracing this process or
/// reading its memory: once it is not dumpable, that takes `CAP_SYS_PTRACE`,
/// which no build container has.
pub fn undumpable() -> io::Result<()> {
    // SAFETY: `prctl(PR_SET_DUMPABLE)` has no memory preconditions.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
