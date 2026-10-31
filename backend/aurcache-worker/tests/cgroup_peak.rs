//! End-to-end check that a build's cgroup reports its peak memory.
//!
//! `#[ignore]` by default, and deliberately: [`Hierarchy::prepare`] moves every
//! process in the worker's own cgroup into a leaf, which on a developer's
//! machine means their login session. It is meant to be run inside a throwaway
//! privileged container:
//!
//! ```sh
//! cargo test -p aurcache-worker --test cgroup_peak --no-run
//! docker run --rm --privileged --cgroupns=private \
//!     -v "$PWD:/w" archlinux /w/backend/target/.../cgroup_peak --ignored
//! ```
//!
//! or, without a container, in a delegated scope of the user's own systemd:
//!
//! ```sh
//! systemd-run --user --scope -p Delegate=yes backend/target/.../cgroup_peak --ignored
//! ```

use aurcache_worker::cgroup::{BuildCgroup, BuildLimits, Hierarchy, OomCause};
use std::os::unix::process::CommandExt;
use std::process::Command;

/// The whole mechanism: prepare the hierarchy, give a build a cgroup, have the
/// child put itself in it before exec, and read back what it used.
///
/// The child allocates ~256 MiB, so the figure has to be at least that and
/// nowhere near the machine's total -- a cgroup that measured the worker
/// instead of the build would report something quite different.
#[test]
#[ignore = "restructures the caller's cgroup; run inside a privileged container"]
fn a_builds_cgroup_reports_its_peak_memory() {
    let hierarchy = Hierarchy::prepare().expect("preparing the cgroup hierarchy");
    let cgroup = hierarchy
        .for_build(4242, &BuildLimits::default())
        .expect("creating the build cgroup");

    let handle = cgroup.procs_handle().expect("opening cgroup.procs");
    let mut cmd = Command::new("/bin/sh");
    // Touch every page so the memory is resident, then exit.
    cmd.args([
        "-c",
        "a=$(head -c 268435456 /dev/zero | tr '\\0' 'x'); echo ${#a} >/dev/null",
    ]);
    // SAFETY: one write to a descriptor opened before the fork.
    unsafe {
        cmd.pre_exec(move || BuildCgroup::join_current_process(&handle));
    }
    let status = cmd.status().expect("running the child");
    assert!(status.success(), "child failed: {status}");

    let peak = cgroup.peak_bytes().expect("memory.peak should be readable");
    assert!(
        peak >= 200 * 1024 * 1024,
        "peak {peak} is below what the child allocated; the child was probably \
         never placed in the cgroup"
    );
    assert!(
        peak < 8 * 1024 * 1024 * 1024,
        "peak {peak} is implausibly large; this looks like the whole machine \
         rather than one build"
    );
}

/// Run `script` under `sh` inside `cgroup`, as a build's processes are.
fn run_in(cgroup: &BuildCgroup, script: &str) -> std::process::ExitStatus {
    let handle = cgroup.procs_handle().expect("opening cgroup.procs");
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", script]);
    // SAFETY: one write to a descriptor opened before the fork.
    unsafe {
        cmd.pre_exec(move || BuildCgroup::join_current_process(&handle));
    }
    cmd.status().expect("running the child")
}

/// Nested limits, and telling them apart afterwards: a build under its own
/// limit killed at the total, and a build killed at its own limit well under
/// the total, each classified as what reached it.
#[test]
#[ignore = "restructures the caller's cgroup; run inside a privileged container or a delegated scope"]
fn a_kill_is_attributed_to_the_total_or_to_the_builds_own_limit() {
    const MIB: u64 = 1024 * 1024;
    // Each child builds a ~500 MiB string, which a 400 MiB total cannot hold.
    let hog = "a=$(head -c 524288000 /dev/zero | tr '\\0' 'x'); echo ${#a} >/dev/null";

    let hierarchy = Hierarchy::prepare().expect("preparing the cgroup hierarchy");
    hierarchy
        .apply_total(&BuildLimits {
            memory_max: Some(400 * MIB),
            swap_max: Some(0),
            cpus: None,
        })
        .expect("applying the total");

    let classify = |cgroup: &BuildCgroup, before: Option<u64>| {
        let during = hierarchy
            .total_ooms()
            .zip(before)
            .map_or(0, |(after, before)| after - before);
        OomCause::classify(cgroup.limit_ooms().unwrap_or(0), during)
    };

    // Under its own 1 GiB, over the 400 MiB total.
    let roomy = hierarchy
        .for_build(
            4243,
            &BuildLimits {
                memory_max: Some(1024 * MIB),
                swap_max: Some(0),
                cpus: None,
            },
        )
        .expect("creating the build cgroup");
    let before = hierarchy.total_ooms();
    assert!(
        !run_in(&roomy, hog).success(),
        "the total should have stopped it"
    );
    assert!(
        roomy.oom_kills().unwrap_or(0) > 0,
        "no OOM kill was recorded"
    );
    assert_eq!(classify(&roomy, before), OomCause::TotalLimit);
    drop(roomy);

    // Over its own 300 MiB, which the total would have allowed.
    let tight = hierarchy
        .for_build(
            4244,
            &BuildLimits {
                memory_max: Some(300 * MIB),
                swap_max: Some(0),
                cpus: None,
            },
        )
        .expect("creating the build cgroup");
    let before = hierarchy.total_ooms();
    assert!(
        !run_in(&tight, hog).success(),
        "its own limit should have stopped it"
    );
    assert!(
        tight.oom_kills().unwrap_or(0) > 0,
        "no OOM kill was recorded"
    );
    assert_eq!(classify(&tight, before), OomCause::BuildLimit);
}
