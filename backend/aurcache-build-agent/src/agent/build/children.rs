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
use std::process::{Command, ExitStatus};
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
}

/// Whether every other process in the container is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quiet {
    /// Nothing but the agent is left.
    Alone,
    /// Something outlived the time it was given to die.
    Lingering,
}

/// Which process killing every other one leaves alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spare {
    /// None: the agent is left alone in the container.
    Nothing,
    /// The worker's SSH link: the process the agent took the link from,
    /// before anything of the job ran. It is spared only while it still has
    /// no parent in this PID namespace, as what the engine starts from
    /// outside has none, so a process that got its number later is not:
    /// what a process in the container starts is its child, or the agent's
    /// once orphaned. Every other exec, an image's health check among them,
    /// is killed like the rest.
    Link { pid: u32 },
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
    let child = command.spawn()?;
    let pid = child.id();
    if let Some(own) = table.own.as_mut() {
        own.insert(pid, None);
    }
    table.started += 1;
    CHANGED.notify_all();
    Ok(Running { pid })
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

/// Kill every other process in the container but those `spare` names, and
/// wait until none is left.
///
/// Only PID 1 may call this: for it, `kill(-1)` reaches exactly the
/// container's other processes, and each of them is its child or becomes one
/// when its parent dies, so having no child left means nothing else runs.
/// The signal is sent again on every round, for a process started while the
/// first was being delivered. Sparing some, each of the others is signalled
/// by its number instead.
pub fn kill_all(limit: Duration, spare: Spare) -> Quiet {
    let deadline = Instant::now() + limit;
    loop {
        match spare {
            Spare::Nothing => {
                // SAFETY: see above; the caller is PID 1 of its own PID
                // namespace.
                unsafe { libc::kill(-1, libc::SIGKILL) };
            }
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

/// The processes `/proc` lists but this one and the one `spare` leaves
/// alone. This also sees a process that entered the container from outside
/// and is nobody's child in here. Without a readable `/proc` there is
/// nothing more to learn, and having no child decides.
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

/// Whether `spare` leaves the process numbered `pid` alone.
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
    use super::{Peek, Spare, collect, parent, peek, spared};
    use std::process::Command;

    /// A process's parent is read as `/proc` has it, and a process that is
    /// gone has none.
    #[test]
    fn a_parent_is_read_from_proc() {
        assert_eq!(
            parent(std::process::id()),
            Some(std::os::unix::process::parent_id())
        );
        assert_eq!(parent(u32::MAX), None);
    }

    /// The link's process is spared by its number only while it has no
    /// parent here: this one has one, as anything started in the container
    /// does, and so does a process that got a gone link's number.
    #[test]
    fn only_a_process_from_outside_is_spared_by_its_number() {
        let mut child = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("running sleep");
        let pid = child.id();
        assert_eq!(parent(pid), Some(std::process::id()));
        assert!(!spared(Spare::Link { pid }, pid));
        assert!(!spared(Spare::Nothing, pid));
        // Nor is a process that is gone.
        assert!(!spared(Spare::Link { pid: u32::MAX }, u32::MAX));
        child.kill().unwrap();
        child.wait().unwrap();
    }

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
