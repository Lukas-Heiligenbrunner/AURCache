//! Every process the agent starts, and every one it inherits.
//!
//! The agent is the container's PID 1, so whatever a build leaves behind
//! becomes its child and stays a zombie until it is waited for. Only the
//! main thread starts and waits for children, one at a time, so waiting for
//! any child cannot take a status that something else is waiting for. When
//! an exec fails, `Command::spawn` collects that child itself, before it
//! returns. The SSH relay's threads start and wait for no process.

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

/// The one process that `kill_all` leaves running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spare {
    Nothing,
    /// The worker's SSH link: the process that the agent took the link from,
    /// before anything of the job ran. It is spared only while it has no
    /// parent in this PID namespace, as a process started by the engine from
    /// outside has none; a process that gets its number later is started in
    /// here, and has one. Any other exec is killed like the rest.
    Link {
        pid: u32,
    },
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

/// Kill every other process in the container but the one `spare` names, and
/// wait until none is left.
///
/// Only PID 1 may call this: for it, `kill(-1)` reaches exactly the
/// container's other processes, and each of them is its child or becomes one
/// when its parent dies, so having no child left means nothing else runs.
/// The signal is sent again on every round, for a process started while the
/// first was being delivered. `kill(-1)` would reach the spared process too,
/// so with one, the others are killed one by one.
pub fn kill_all(limit: Duration, spare: Spare) -> Quiet {
    let deadline = Instant::now() + limit;
    loop {
        match spare {
            // SAFETY: see above; the caller is PID 1 of its own PID namespace.
            Spare::Nothing => unsafe {
                libc::kill(-1, libc::SIGKILL);
            },
            Spare::Link { .. } => {
                for pid in others(spare) {
                    // SAFETY: a signal to a process of this namespace.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
        if childless() && others(spare).is_empty() {
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

/// The processes that `/proc` lists, but this one and the one `spare` names.
/// This also sees a process that entered the container from outside and is
/// nobody's child in here. Without a readable `/proc` there is nothing more
/// to learn, and having no child decides.
fn others(spare: Spare) -> Vec<libc::pid_t> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let own = std::process::id();
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != own && !spared(spare, pid))
        .filter_map(|pid| libc::pid_t::try_from(pid).ok())
        .collect()
}

fn spared(spare: Spare, pid: u32) -> bool {
    match spare {
        Spare::Nothing => false,
        Spare::Link { pid: link } => pid == link && parent(pid) == Some(0),
    }
}

/// A process's parent, as `/proc` gives it: 0 for one whose parent is
/// outside this PID namespace. `None` once it is gone.
fn parent(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|ppid| ppid.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::{Spare, parent, spared};
    use std::process::Command;

    /// Only a process whose parent is outside this PID namespace is spared:
    /// a child here has a parent, and a process that is gone has none.
    #[test]
    fn only_a_process_from_outside_is_spared() {
        assert_eq!(
            parent(std::process::id()),
            Some(std::os::unix::process::parent_id())
        );
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();
        assert_eq!(parent(pid), Some(std::process::id()));
        assert!(!spared(Spare::Link { pid }, pid));
        assert!(!spared(Spare::Nothing, pid));
        assert_eq!(parent(u32::MAX), None);
        assert!(!spared(Spare::Link { pid: u32::MAX }, u32::MAX));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
