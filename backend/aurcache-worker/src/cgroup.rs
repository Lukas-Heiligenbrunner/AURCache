//! A cgroup per build: its peak memory, its limits, and how it is killed.
//!
//! cgroup v2 records `memory.peak` for a cgroup and everything in it, so giving
//! each build its own answers "how much did this build need" exactly, with no
//! sampling and nothing to poll. The build is a tree -- `makechrootpkg` runs
//! `systemd-nspawn` runs `makepkg` runs one compiler per core -- and every
//! descendant inherits the cgroup its parent was placed in, so one number
//! covers all of them. The same property makes it the place for the build's
//! `memory.max`/`cpu.max` ([`BuildLimits`]) and for `cgroup.kill`.
//!
//! ## Layout
//!
//! ```text
//! <the worker's cgroup>/        e.g. system.slice/aurcache-worker.service
//! ├── worker/                   the worker process itself
//! └── builds/                   every build together: the total limits
//!     ├── build-1010/           one build: its own limits
//!     └── build-1011/
//! ```
//!
//! cgroup limits nest: `builds/`'s `memory.max` caps the sum of the builds
//! under it, and each build's own `memory.max` still applies to that build.
//! Putting the total on `builds/` rather than on the worker's cgroup keeps the
//! worker process out of it, so reaching the total can only ever kill a build,
//! and it tells the two limits apart afterwards (see [`OomCause`]).
//!
//! ## Why the worker has to prepare its own hierarchy
//!
//! cgroup v2 refuses to enable a controller in `cgroup.subtree_control` while
//! processes sit directly in that cgroup (`EBUSY`), and both deployments hand
//! the worker a cgroup it is already running in. So at startup the worker moves
//! itself into a leaf and enables `memory` for its siblings -- the standard
//! delegation dance, and what systemd's own documentation tells a delegated
//! service to do.
//!
//! Both supported deployments can do this:
//!
//!  * **The container images** run privileged, which `mkarchroot` and
//!    `arch-nspawn` already require, and that makes `/sys/fs/cgroup` writable.
//!  * **A native install** runs under `aurcache-worker.service`, which sets
//!    `Delegate=yes` so systemd hands the subtree over rather than managing it.
//!
//! Where the hierarchy cannot be prepared, builds still run and simply report
//! no figure; nothing here is on the path that produces packages.

use anyhow::{Context, Result, bail};
use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

/// Where cgroup v2 is mounted. Fixed by convention and by systemd.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The leaf the worker moves itself into so its own cgroup can host children.
const SELF_LEAF: &str = "worker";

/// The cgroup holding every build, where the total limits go.
const BUILDS_DIR: &str = "builds";

/// The prepared cgroup the worker creates each build's cgroup under.
pub struct Hierarchy {
    /// The worker's own cgroup, holding [`SELF_LEAF`] and [`BUILDS_DIR`].
    base: PathBuf,
    /// `base/builds`, the parent of every build's cgroup.
    builds: PathBuf,
}

impl Hierarchy {
    /// Move this process into a leaf and enable the memory controller for its
    /// siblings, so a per-build cgroup can be created and measured.
    ///
    /// Idempotent: a worker that restarts into an already-prepared hierarchy
    /// finds the controller enabled and only re-parents itself.
    pub fn prepare() -> Result<Self> {
        let base = own_cgroup_dir()?;

        // Nothing can be enabled while this cgroup holds processes, so the move
        // has to come first -- including this process, which is why the leaf
        // exists at all.
        let leaf = base.join(SELF_LEAF);
        fs::create_dir_all(&leaf).with_context(|| format!("creating {}", leaf.display()))?;
        for pid in read_procs(&base.join("cgroup.procs"))? {
            // Best-effort per pid: a process that exits between the read and
            // the write is gone, which is the outcome the move wanted anyway.
            let _ = fs::write(leaf.join("cgroup.procs"), pid.to_string());
        }

        let builds = base.join(BUILDS_DIR);
        let hierarchy = Self { base, builds };
        enable_controller(&hierarchy.base, "memory")?;
        // In this order: a controller can only be enabled for `builds/`'s
        // children once `builds/` itself has it from the level above.
        fs::create_dir_all(&hierarchy.builds)
            .with_context(|| format!("creating {}", hierarchy.builds.display()))?;
        enable_controller(&hierarchy.builds, "memory")?;
        Ok(hierarchy)
    }

    /// Enable the `cpu` controller for the builds, which `cpu.max` needs.
    ///
    /// Only when a CPU limit is configured, and not as part of [`Self::prepare`]:
    /// with the controller on, builds compete by cgroup rather than by process,
    /// so a `-j24` build and a single-threaded one get equal time under
    /// contention. That is right when builds are capped anyway and a change
    /// nobody asked for when they are not.
    pub fn enable_cpu(&self) -> Result<()> {
        // Both levels: `builds/` needs it for a total `cpu.max`, and each build
        // needs `builds/` to pass it down for its own.
        enable_controller(&self.base, "cpu")?;
        enable_controller(&self.builds, "cpu")
    }

    /// Apply the limits for all builds together, on `builds/`.
    ///
    /// At startup, and again whenever the server delivers different totals:
    /// they bound what the builds already running use between them, so a
    /// change takes effect at once. Unset limits are written as `max`, so
    /// removing one really removes it rather than leaving the previous value
    /// in place.
    pub fn apply_total(&self, limits: &BuildLimits) -> Result<()> {
        write_limits(&self.builds, limits, UnsetLimit::Reset)
    }

    /// How many times the total memory limit has been reached, from
    /// `builds/memory.events.local`: the local file, because the hierarchical
    /// one also counts every build reaching its own limit.
    ///
    /// Cumulative for as long as `builds/` exists, so a build compares the value
    /// from its start with the one from its end.
    #[must_use]
    pub fn total_ooms(&self) -> Option<u64> {
        let raw = fs::read_to_string(self.builds.join("memory.events.local")).ok()?;
        parse_event(&raw, "oom")
    }

    /// Create the cgroup for one build, with `limits` applied.
    ///
    /// A limit that cannot be applied fails this, and with it the build: a
    /// configured limit is there to keep one build from taking the machine
    /// down, and running without it is the outcome it exists to prevent.
    pub fn for_build(&self, build_id: i32, limits: &BuildLimits) -> Result<BuildCgroup> {
        let dir = self.builds.join(format!("build-{build_id}"));
        // A cgroup left by a previous attempt at the same build is empty by
        // now; removing it is what keeps `memory.peak` about this attempt.
        let _ = remove_cgroup_tree(&dir);
        // On demand rather than only at startup: a CPU limit can arrive from
        // the server long after the worker started without one, and `cpu.max`
        // cannot be written until the controller is on. A no-op once it is.
        if limits.cpus.is_some() {
            self.enable_cpu()?;
        }
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let cgroup = BuildCgroup { dir };
        write_limits(&cgroup.dir, limits, UnsetLimit::Preserve)?;
        Ok(cgroup)
    }
}

fn enable_controller(cgroup: &Path, controller: &str) -> Result<()> {
    let control = cgroup.join("cgroup.subtree_control");
    if fs::read_to_string(&control)
        .unwrap_or_default()
        .split_whitespace()
        .any(|c| c == controller)
    {
        return Ok(());
    }
    fs::write(&control, format!("+{controller}")).with_context(|| {
        format!(
            "enabling the {controller} controller in {} -- the worker needs a \
             writable cgroup subtree (privileged container, or a systemd \
             unit with Delegate=yes)",
            control.display()
        )
    })
}

/// Resource limits for a cgroup, from the worker's configuration: each build's
/// (`WORKER_BUILD_*`), or all builds' together (`WORKER_TOTAL_BUILD_*`).
///
/// Per build, `WORKER_CONCURRENCY` builds can each use this much; the total
/// bounds what they use between them. Neither includes the worker process, so
/// a cap on literally everything still belongs on what runs the worker --
/// `MemoryMax=` on the systemd unit, `--memory` on the container.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BuildLimits {
    /// `memory.max`, in bytes: the RAM the build may use.
    pub memory_max: Option<u64>,
    /// `memory.swap.max`, in bytes: the swap it may use besides.
    ///
    /// `memory.max` alone does not bound a build on a machine with swap: the
    /// kernel pushes what is over the limit out to swap instead of killing
    /// anything. Measured on a worker with zram, a 900 MiB allocation under a
    /// 300 MiB `memory.max` simply succeeded. So a memory limit brings a swap
    /// limit of `0` with it unless one is configured, and reaching the limit
    /// then ends in the OOM kill it is expected to.
    pub swap_max: Option<u64>,
    /// `cpu.max`, in CPUs: `2.5` is two and a half cores' worth of time per
    /// period, spread over as many cores as the build uses.
    pub cpus: Option<f64>,
}

impl BuildLimits {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.memory_max.is_none() && self.swap_max.is_none() && self.cpus.is_none()
    }
}

/// The scheduler period `cpu.max` is expressed against, in microseconds. The
/// kernel's own default.
const CPU_PERIOD_US: u64 = 100_000;

/// `cpu.max` for a number of CPUs: quota and period, in microseconds.
///
/// The kernel refuses a quota under a millisecond, so a very small fraction is
/// raised to that rather than turned into a failed build.
fn cpu_max(cpus: f64) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let quota = ((cpus * CPU_PERIOD_US as f64).round() as u64).max(1_000);
    format!("{quota} {CPU_PERIOD_US}")
}

/// One build's cgroup. Removed when dropped.
pub struct BuildCgroup {
    dir: PathBuf,
}

/// Write `limits` into a cgroup. Everything placed in it afterwards -- a build
/// and the container nspawn keeps in it, or every build under `builds/` -- is
/// bound by them.
///
/// `UnsetLimit::Reset` writes `max` for a limit that is not configured. A build's
/// cgroup is new, where every file already says `max`, so it has nothing to
/// reset; `builds/` outlives the worker process and would otherwise keep a
/// limit from a previous run's configuration.
fn write_limits(dir: &Path, limits: &BuildLimits, unset: UnsetLimit) -> Result<()> {
    let value = |limit: Option<String>| match limit {
        Some(v) => Some(v),
        None if unset == UnsetLimit::Reset => Some("max".to_string()),
        None => None,
    };
    if let Some(v) = value(limits.memory_max.map(|b| b.to_string())) {
        fs::write(dir.join("memory.max"), v)
            .with_context(|| format!("setting memory.max in {}", dir.display()))?;
    }
    if let Some(v) = value(limits.swap_max.map(|b| b.to_string())) {
        match fs::write(dir.join("memory.swap.max"), v) {
            Ok(()) => {}
            // No swap accounting in this kernel: nothing to limit it with, and
            // refusing every build over it would help no one.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    "no memory.swap.max in {}; swap is not limited",
                    dir.display()
                );
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("setting memory.swap.max in {}", dir.display()));
            }
        }
    }
    match limits.cpus {
        Some(cpus) => fs::write(dir.join("cpu.max"), cpu_max(cpus)).with_context(|| {
            format!(
                "setting cpu.max in {} -- is the cpu controller delegated to the worker?",
                dir.display()
            )
        })?,
        // Only where the file exists: without a CPU limit anywhere the
        // controller is not enabled, and there is no `cpu.max` to reset.
        None if unset == UnsetLimit::Reset && dir.join("cpu.max").exists() => {
            fs::write(dir.join("cpu.max"), format!("max {CPU_PERIOD_US}"))
                .with_context(|| format!("resetting cpu.max in {}", dir.display()))?;
        }
        None => {}
    }
    Ok(())
}

/// What to do with a limit omitted from the new configuration.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnsetLimit {
    Preserve,
    Reset,
}

/// Which limit an OOM kill in a build came from.
///
/// Told apart by where the kernel counts the event: `oom` in a cgroup's
/// `memory.events` is the number of times *its own* limit (or one below it)
/// was reached, while `oom_kill` counts its processes killed whatever limit
/// was reached. So a build killed at the total has kills but no `oom` of its
/// own, and `builds/` records the `oom` instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OomCause {
    /// The build reached its own `memory.max`.
    BuildLimit,
    /// The builds together reached the total on `builds/`.
    TotalLimit,
    /// Neither: a limit above the worker (the systemd unit, the container) or
    /// the machine itself ran out.
    Elsewhere,
}

impl OomCause {
    /// Classify from the build's own `oom` count and how much the total's
    /// count grew while the build ran.
    #[must_use]
    pub const fn classify(build_ooms: u64, total_ooms_during: u64) -> Self {
        if build_ooms > 0 {
            Self::BuildLimit
        } else if total_ooms_during > 0 {
            Self::TotalLimit
        } else {
            Self::Elsewhere
        }
    }
}

impl BuildCgroup {
    /// How many processes in this build the kernel's OOM killer ended.
    ///
    /// `memory.events` counts the whole subtree, so a compiler killed inside the
    /// container counts here. `None` when it cannot be read.
    #[must_use]
    pub fn oom_kills(&self) -> Option<u64> {
        let raw = fs::read_to_string(self.dir.join("memory.events")).ok()?;
        parse_event(&raw, "oom_kill")
    }

    /// How many times this build reached its own memory limit. See [`OomCause`].
    #[must_use]
    pub fn limit_ooms(&self) -> Option<u64> {
        let raw = fs::read_to_string(self.dir.join("memory.events")).ok()?;
        parse_event(&raw, "oom")
    }

    /// An open handle to `cgroup.procs`, for the child to place itself into.
    ///
    /// Opened here rather than in the child because opening a file after
    /// `fork` in a threaded process is not async-signal-safe. The child writes
    /// a single byte to this descriptor; see [`Self::join_current_process`].
    pub fn procs_handle(&self) -> Result<OwnedFd> {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(self.dir.join("cgroup.procs"))
            .with_context(|| format!("opening cgroup.procs in {}", self.dir.display()))?;
        Ok(file.into())
    }

    /// Move the calling process into the cgroup behind `handle`.
    ///
    /// Called between `fork` and `exec`, where almost nothing is legal: this is
    /// one `write` of one byte to a descriptor that was already open. Writing
    /// `0` rather than a pid is what keeps it that way -- the kernel reads it
    /// as "whoever is writing" -- so there is no formatting and no allocation.
    ///
    /// # Safety
    /// Must only be called from a `pre_exec` hook, with `handle` opened before
    /// the fork.
    pub unsafe fn join_current_process(handle: &OwnedFd) -> std::io::Result<()> {
        // SAFETY: a single write to an open descriptor is async-signal-safe.
        let written = unsafe { libc::write(handle.as_raw_fd(), c"0".as_ptr().cast(), 1) };
        if written == 1 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// The high-water mark of this cgroup's memory, in bytes.
    ///
    /// `None` when the kernel does not report one: `memory.peak` needs Linux
    /// 5.19 or newer. Distinct from a build that used nothing, which cannot
    /// happen, so the page can show the two differently.
    #[must_use]
    pub fn peak_bytes(&self) -> Option<i64> {
        let raw = fs::read_to_string(self.dir.join("memory.peak")).ok()?;
        parse_peak(&raw)
    }

    /// Kill every process in this cgroup, including descendants.
    ///
    /// `cgroup.kill` is v2's recursive kill: it signals the whole tree — the
    /// nspawn child, `makechrootpkg`, each compiler — in one write, which is
    /// exactly the tree a build is. Only the leaf's processes are listed by
    /// `cgroup.procs`; `cgroup.kill` walks the descendants for us.
    ///
    /// This is the primary kill path for an aborted (cancelled) or timed-out
    /// build: `child.start_kill()` would only take `makechrootpkg`, and the
    /// processes it spawned (possibly in a systemd-managed scope that escapes
    /// the process group) would linger. Falls back to the outside caller.
    pub fn kill(&self) -> Result<()> {
        fs::write(self.dir.join("cgroup.kill"), "1")
            .with_context(|| format!("writing cgroup.kill in {}", self.dir.display()))
    }
}

impl Drop for BuildCgroup {
    fn drop(&mut self) {
        // Only ever empty by now -- the build's processes have been waited on.
        // A cgroup that somehow still holds one cannot be removed, and leaving
        // it is better than blocking the worker: `for_build` clears it next
        // time the same build number comes round.
        if let Err(e) = remove_cgroup_tree(&self.dir) {
            tracing::debug!("could not remove {}: {e}", self.dir.display());
        }
    }
}

/// Remove a cgroup and every cgroup beneath it, deepest first.
///
/// cgroupfs refuses `rmdir` on a cgroup that still has child cgroups, and a
/// build's has two: systemd-nspawn, kept in the build's cgroup with
/// `--keep-unit`, creates `payload` and `supervisor` inside it. Removing only
/// the top left every build's cgroup behind for good. The files in a cgroup
/// are the kernel's and go with its directory, so only directories are
/// removed -- which is also all `rmdir` can do, so nothing outside cgroupfs is
/// at risk.
fn remove_cgroup_tree(dir: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)?.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            remove_cgroup_tree(&entry.path())?;
        }
    }
    fs::remove_dir(dir)
}

/// This process's own cgroup directory.
fn own_cgroup_dir() -> Result<PathBuf> {
    let raw = fs::read_to_string("/proc/self/cgroup").context("reading /proc/self/cgroup")?;
    let Some(rel) = parse_own_cgroup(&raw) else {
        bail!("no cgroup v2 entry in /proc/self/cgroup; this needs a unified hierarchy");
    };
    Ok(Path::new(CGROUP_ROOT).join(rel.trim_start_matches('/')))
}

/// The path from the cgroup v2 line of a `/proc/<pid>/cgroup`.
///
/// The unified hierarchy is the entry with an empty controller list, written
/// `0::<path>`. Any other line is a v1 controller and says nothing about where
/// `memory.peak` would be.
fn parse_own_cgroup(raw: &str) -> Option<&str> {
    raw.lines().find_map(|l| l.strip_prefix("0::"))
}

fn read_procs(path: &Path) -> Result<Vec<u32>> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(raw.lines().filter_map(|l| l.trim().parse().ok()).collect())
}

/// `memory.peak` is a single decimal number of bytes.
fn parse_peak(raw: &str) -> Option<i64> {
    raw.trim().parse().ok()
}

/// One counter from `memory.events`, which is `key value` per line.
fn parse_event(raw: &str, wanted: &str) -> Option<u64> {
    raw.lines().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        (key == wanted).then(|| value.trim().parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_max_is_quota_over_the_default_period() {
        assert_eq!(cpu_max(8.0), "800000 100000");
        assert_eq!(cpu_max(2.5), "250000 100000");
        assert_eq!(cpu_max(0.25), "25000 100000");
        // Below the kernel's minimum quota, raised rather than refused.
        assert_eq!(cpu_max(0.001), "1000 100000");
    }

    #[test]
    fn oom_counters_come_from_memory_events() {
        let events = "low 0\nhigh 0\nmax 12\noom 3\noom_kill 2\noom_group_kill 0\n";
        assert_eq!(parse_event(events, "oom_kill"), Some(2));
        // `oom` is its own key, not a prefix match on `oom_kill`.
        assert_eq!(parse_event(events, "oom"), Some(3));
        assert_eq!(parse_event("low 0\nhigh 0\n", "oom_kill"), None);
    }

    /// The counts as measured on Linux 7.2 with a 400M parent and two children:
    /// a child killed at the parent's limit had `oom_kill 1` and `oom 0`, the
    /// parent's `memory.events.local` `oom 1`; a child killed at its own had
    /// `oom 1`, and the parent's local count did not move.
    #[test]
    fn an_oom_is_attributed_to_the_limit_whose_count_moved() {
        assert_eq!(OomCause::classify(0, 1), OomCause::TotalLimit);
        assert_eq!(OomCause::classify(1, 0), OomCause::BuildLimit);
        // Both at once: the build's own limit was reached, which is what its
        // operator has to change for it.
        assert_eq!(OomCause::classify(1, 1), OomCause::BuildLimit);
        assert_eq!(OomCause::classify(0, 0), OomCause::Elsewhere);
    }

    /// Limits land in the files the kernel reads them from. A plain directory
    /// stands in for cgroupfs.
    #[test]
    fn limits_are_written_where_the_kernel_reads_them() {
        let tmp = tempfile::tempdir().unwrap();
        write_limits(
            tmp.path(),
            &BuildLimits {
                memory_max: Some(32 * 1024 * 1024 * 1024),
                swap_max: Some(0),
                cpus: Some(6.0),
            },
            UnsetLimit::Preserve,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("memory.max")).unwrap(),
            "34359738368"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("memory.swap.max")).unwrap(),
            "0"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("cpu.max")).unwrap(),
            "600000 100000"
        );
        // No limit, no file: the kernel's `max` stays in place.
        let unlimited = tempfile::tempdir().unwrap();
        write_limits(
            unlimited.path(),
            &BuildLimits::default(),
            UnsetLimit::Preserve,
        )
        .unwrap();
        assert!(!unlimited.path().join("memory.max").exists());
    }

    /// `builds/` outlives the worker process, so a total removed from the
    /// configuration has to be written back to `max`, not left as it was.
    #[test]
    fn an_unset_total_resets_what_a_previous_run_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("memory.max"), "1073741824").unwrap();
        fs::write(tmp.path().join("memory.swap.max"), "0").unwrap();
        fs::write(tmp.path().join("cpu.max"), "200000 100000").unwrap();

        write_limits(tmp.path(), &BuildLimits::default(), UnsetLimit::Reset).unwrap();

        assert_eq!(
            fs::read_to_string(tmp.path().join("memory.max")).unwrap(),
            "max"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("memory.swap.max")).unwrap(),
            "max"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("cpu.max")).unwrap(),
            "max 100000"
        );
        // No cpu controller, no file: nothing is created where there was none.
        let bare = tempfile::tempdir().unwrap();
        write_limits(bare.path(), &BuildLimits::default(), UnsetLimit::Reset).unwrap();
        assert!(!bare.path().join("cpu.max").exists());
    }

    /// The shape nspawn leaves under a build's cgroup. A plain directory tree
    /// stands in for cgroupfs, where the kernel's files go with each directory.
    #[test]
    fn a_build_cgroup_is_removed_with_the_cgroups_nspawn_made_in_it() {
        let tmp = tempfile::tempdir().unwrap();
        let build = tmp.path().join("build-997");
        fs::create_dir_all(build.join("payload/nested")).unwrap();
        fs::create_dir_all(build.join("supervisor")).unwrap();

        remove_cgroup_tree(&build).unwrap();

        assert!(!build.exists());
        assert!(tmp.path().exists());
    }

    /// The unified hierarchy is the `0::` line; v1 controller lines sit
    /// alongside it on a hybrid host and mean something else entirely.
    #[test]
    fn the_unified_hierarchy_is_the_zero_line() {
        let hybrid = "12:pids:/system.slice/x.service\n1:name=systemd:/user.slice\n0::/system.slice/aurcache-worker.service\n";
        assert_eq!(
            parse_own_cgroup(hybrid).unwrap(),
            "/system.slice/aurcache-worker.service"
        );
    }

    /// A container with a private cgroup namespace sees itself at the root.
    #[test]
    fn a_container_sees_itself_at_the_root() {
        assert_eq!(parse_own_cgroup("0::/\n").unwrap(), "/");
    }

    /// A v1-only host has no `0::` line, and there is no `memory.peak` to find.
    #[test]
    fn a_v1_only_hierarchy_has_no_unified_entry() {
        assert!(parse_own_cgroup("12:pids:/x\n1:name=systemd:/y\n").is_none());
    }

    #[test]
    fn peak_is_a_plain_byte_count() {
        assert_eq!(parse_peak("1552384\n").unwrap(), 1_552_384);
        assert!(parse_peak("").is_none());
    }
}
