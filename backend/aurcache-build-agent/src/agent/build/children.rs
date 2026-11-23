//! Every process the agent starts, and every one it inherits.
//!
//! The agent is the container's PID 1, so whatever a build leaves behind
//! becomes its child and stays a zombie until it is waited for. The agent
//! has one thread, and starts and waits for each child in turn, so waiting
//! for any child cannot take a status that something else is waiting for.
//! When an exec fails, `Command::spawn` collects that child itself, before
//! it returns.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

/// How often the processes are counted while they are being killed.
const POLL: Duration = Duration::from_millis(10);

/// Whether every other process in the container is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quiet {
    /// Nothing but the agent is left.
    Alone,
    /// Something outlived the time it was given to die.
    Lingering,
}

/// Start a child, to be collected with [`wait`].
pub fn spawn(command: &mut Command) -> io::Result<u32> {
    Ok(command.spawn()?.id())
}

/// Wait for `pid`, a child started with [`spawn`], collecting whatever else
/// ends first.
pub fn wait(pid: u32) -> io::Result<ExitStatus> {
    let pid = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a valid place for `waitpid` to write to.
        let got = unsafe { libc::waitpid(-1, &raw mut status, 0) };
        if got == pid {
            return Ok(ExitStatus::from_raw(status));
        }
        if got < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

/// Kill every other process in the container, and wait until none is left.
///
/// Only PID 1 may call this: for it, `kill(-1)` reaches exactly the
/// container's other processes, and each of them is its child or becomes one
/// when its parent dies, so having no child left means nothing else runs.
/// The signal is sent again on every round, for a process started while the
/// first was being delivered.
pub fn kill_all(limit: Duration) -> Quiet {
    let deadline = Instant::now() + limit;
    loop {
        // SAFETY: see above; the caller is PID 1 of its own PID namespace.
        unsafe { libc::kill(-1, libc::SIGKILL) };
        if childless() && alone() {
            return Quiet::Alone;
        }
        if Instant::now() >= deadline {
            return Quiet::Lingering;
        }
        std::thread::sleep(POLL);
    }
}

/// Collect every child that ended, and say whether none is left.
fn childless() -> bool {
    loop {
        // SAFETY: a null status is allowed, and asks for none.
        let got = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if got == 0 {
            return false;
        }
        if got < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return true;
        }
    }
}

/// Whether `/proc` lists no process but this one. This also sees a process
/// that entered the container from outside and is nobody's child in here.
/// Without a readable `/proc` there is nothing more to learn, and having no
/// child decides.
fn alone() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return true;
    };
    let own = std::process::id().to_string();
    !entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name != own.as_str() && !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())
    })
}
