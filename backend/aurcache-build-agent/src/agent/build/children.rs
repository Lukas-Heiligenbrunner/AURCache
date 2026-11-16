//! Every process the agent starts, and every one it inherits.
//!
//! The agent is the container's PID 1, so whatever a build leaves behind
//! becomes its child and stays a zombie until it is waited for. One thread
//! does all the waiting: it takes each process that ended, keeps the status
//! of those the agent started for whoever asks, and forgets the rest. Nothing
//! else may wait for a child, since a status can only be collected once.

use std::collections::HashMap;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{ChildStdin, Command, ExitStatus};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How long the reaper sleeps with no child at all before it looks again
/// for one it inherited.
const IDLE: Duration = Duration::from_secs(1);

/// How often the processes are counted while they are being killed.
const POLL: Duration = Duration::from_millis(10);

struct Table {
    /// The agent's own children: `None` while one runs, its status once it
    /// ended, gone once someone took it.
    own: Option<HashMap<u32, Option<ExitStatus>>>,
    /// How many were ever started, which tells the reaper one came since it
    /// last found none.
    started: u64,
}

static TABLE: Mutex<Table> = Mutex::new(Table {
    own: None,
    started: 0,
});
static CHANGED: Condvar = Condvar::new();

fn table() -> MutexGuard<'static, Table> {
    TABLE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A child of the agent's that has not been waited for.
pub struct Running {
    pub pid: u32,
    /// The write end of its stdin, when it was started with a pipe there.
    pub stdin: Option<ChildStdin>,
}

/// Whether every other process in the container is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quiet {
    /// Nothing but the agent is left.
    Alone,
    /// Something outlived the time it was given to die.
    Lingering,
}

/// Start the thread that waits for every child. Call once, before `spawn`.
pub fn start() {
    table().own = Some(HashMap::new());
    std::thread::spawn(reap);
}

/// Start a child. Its status is collected with [`wait`], and only there.
///
/// The table stays locked until the child is in it: the reaper locks it
/// before it collects anything, so it never meets a child it cannot place,
/// nor takes the status of one whose start failed, which the standard
/// library collects itself.
pub fn spawn(command: &mut Command) -> io::Result<Running> {
    let mut table = table();
    let mut child = command.spawn()?;
    let pid = child.id();
    if let Some(own) = table.own.as_mut() {
        own.insert(pid, None);
    }
    table.started += 1;
    CHANGED.notify_all();
    Ok(Running {
        pid,
        stdin: child.stdin.take(),
    })
}

/// Wait for a child started with [`spawn`] to end.
pub fn wait(pid: u32) -> ExitStatus {
    let mut table = table();
    loop {
        if let Some(status) = taken(&mut table, pid) {
            return status;
        }
        table = CHANGED.wait(table).unwrap_or_else(PoisonError::into_inner);
    }
}

/// Wait for a child for at most `limit`; `None` when it is still running.
pub fn wait_within(pid: u32, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    let mut table = table();
    loop {
        if let Some(status) = taken(&mut table, pid) {
            return Some(status);
        }
        let left = deadline.checked_duration_since(Instant::now())?;
        table = CHANGED
            .wait_timeout(table, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

/// Kill one child and collect it.
pub fn kill(pid: u32) -> ExitStatus {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: a signal to a child that has not been collected, so the
        // pid is still its own.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    wait(pid)
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

/// The status of a child that ended, removing it from the table.
fn taken(table: &mut Table, pid: u32) -> Option<ExitStatus> {
    let own = table.own.as_mut()?;
    let status = (*own.get(&pid)?)?;
    own.remove(&pid);
    Some(status)
}

/// What a look at the children found.
enum Peek {
    /// This one ended and has not been collected.
    Ended(libc::pid_t),
    /// There is no child at all.
    Childless,
    /// Children are running and none has ended.
    Running,
}

/// Look for a child that ended, without collecting it.
fn peek(flags: libc::c_int) -> Peek {
    loop {
        // SAFETY: `info` is a valid, zeroed `siginfo_t` for `waitid` to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let got = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT | flags,
            )
        };
        if got == 0 {
            // SAFETY: `waitid` succeeded, so the child fields are the ones
            // that are set; with `WNOHANG` a pid of 0 says none has ended.
            let pid = unsafe { info.si_pid() };
            return if pid == 0 {
                Peek::Running
            } else {
                Peek::Ended(pid)
            };
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Peek::Childless;
        }
    }
}

/// Collect one child that ended. `None` when someone else already did.
fn collect(pid: libc::pid_t) -> Option<ExitStatus> {
    let mut status: libc::c_int = 0;
    // SAFETY: `status` is a valid place for `waitpid` to write to.
    let got = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
    (got == pid).then(|| ExitStatus::from_raw(status))
}

/// The reaper: collect every child that ends, for as long as the agent runs.
fn reap() {
    loop {
        let seen = table().started;
        match peek(0) {
            Peek::Ended(pid) => {
                let mut table = table();
                let Some(status) = collect(pid) else { continue };
                let slot = u32::try_from(pid)
                    .ok()
                    .and_then(|pid| table.own.as_mut()?.get_mut(&pid));
                if let Some(slot) = slot {
                    *slot = Some(status);
                    CHANGED.notify_all();
                }
            }
            Peek::Childless | Peek::Running => {
                let table = table();
                drop(
                    CHANGED
                        .wait_timeout_while(table, IDLE, |table| table.started == seen)
                        .unwrap_or_else(PoisonError::into_inner),
                );
            }
        }
    }
}

/// Whether the agent has no child, running or ended.
fn childless() -> bool {
    matches!(peek(libc::WNOHANG), Peek::Childless)
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

#[cfg(test)]
mod tests {
    use super::{Peek, collect, peek};
    use std::process::Command;

    /// A child that ended is seen without being collected, and collected
    /// once. Other tests have children of their own, which are looked at and
    /// left alone.
    #[test]
    #[allow(clippy::zombie_processes)] // collected below, by `collect`
    fn an_ended_child_is_seen_then_collected_once() {
        let child = Command::new("true").spawn().expect("running true");
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        while !matches!(peek(0), Peek::Ended(seen) if seen == pid) {
            std::thread::yield_now();
        }
        // Still there: looking did not take it.
        assert!(collect(pid).is_some_and(|status| status.success()));
        assert!(collect(pid).is_none());
    }
}
