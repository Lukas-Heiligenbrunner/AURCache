//! The pool against a real kernel: loop devices, btrfs, simple quotas.
//!
//! These need root through `sudo -n` and make real mounts, so they only run
//! when asked to: `AURCACHE_POOL_TESTS=1 cargo test -p aurcache-chroot`. Their
//! images go under Cargo's target directory rather than `/tmp`, which is often
//! a size-limited tmpfs.

use aurcache_chroot::{Backing, Owner, Pool, PoolConfig, QgroupId, Resize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const MIB: u64 = 1 << 20;

fn enabled() -> bool {
    if std::env::var_os("AURCACHE_POOL_TESTS").is_none() {
        eprintln!("skipping: set AURCACHE_POOL_TESTS=1 to run pool tests (needs sudo)");
        return false;
    }
    true
}

fn owner() -> Owner {
    Owner::current()
}

fn sudo(args: &[&str]) -> std::process::Output {
    Command::new("sudo").arg("-n").args(args).output().unwrap()
}

fn sudo_ok(args: &[&str]) {
    let out = sudo(args);
    assert!(
        out.status.success(),
        "sudo {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A scratch directory for one test, removed (with anything mounted in it)
/// when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("pool-{name}"));
        let scratch = Self(dir);
        scratch.cleanup();
        std::fs::create_dir_all(&scratch.0).unwrap();
        scratch
    }

    fn cleanup(&self) {
        let dir = self.0.display().to_string();
        // Unmount anything under the directory, deepest first, then detach
        // loop devices backed by files in it.
        let script = format!(
            r#"for m in $(findmnt -rn -o TARGET | grep "^{dir}" | sort -r); do umount -l "$m"; done
               for f in {dir}/*.img; do [ -e "$f" ] && losetup -j "$f" | cut -d: -f1 | xargs -r -n1 losetup -d; done
               rm -rf "{dir}""#
        );
        let _ = sudo(&["sh", "-c", &script]);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn config(scratch: &Scratch, total: u64) -> PoolConfig {
    PoolConfig {
        backing: Backing::Image {
            path: scratch.0.join("pool.img"),
            reserve: false,
        },
        mountpoint: scratch.0.join("pool"),
        total,
        owner: owner(),
    }
}

/// Stand in for `mkarchroot`: a base subvolume with some content.
async fn make_base(pool: &Pool, megabytes: u64) {
    let root = pool.root();
    sudo_ok(&["btrfs", "subvolume", "create", root.to_str().unwrap()]);
    sudo_ok(&["mkdir", "-p", root.join("usr").to_str().unwrap()]);
    sudo_ok(&[
        "sh",
        "-c",
        &format!(
            "head -c {megabytes}M /dev/urandom > {}",
            root.join("usr/blob").display()
        ),
    ]);
    pool.charge_to_total(&root).await.unwrap();
}

/// `fallocate` as the worker, into a directory it owns. Returns the error.
fn fallocate(path: &Path, bytes: u64) -> Option<String> {
    let out = Command::new("fallocate")
        .args(["-l", &bytes.to_string()])
        .arg(path)
        .output()
        .unwrap();
    (!out.status.success()).then(|| String::from_utf8_lossy(&out.stderr).trim().to_string())
}

/// Write `bytes` of incompressible data as the worker; the error, if any.
fn write(path: &Path, bytes: u64) -> Option<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "head -c {bytes} /dev/urandom > '{}' && sync -f '{0}'",
            path.display()
        ))
        .output()
        .unwrap();
    (!out.status.success()).then(|| String::from_utf8_lossy(&out.stderr).trim().to_string())
}

fn sync(pool: &Pool) {
    sudo_ok(&["btrfs", "filesystem", "sync", pool.path().to_str().unwrap()]);
}

/// Marker files in a chroot, all holding `generation`: a snapshot that mixes
/// generations was torn by a swap mid-read. As root: the base is root-owned.
fn write_gen(root: &Path, generation: u64) {
    let script = (0..8)
        .map(|i| format!("echo {generation} > '{}'/gen-{i}", root.display()))
        .collect::<Vec<_>>()
        .join(" && ");
    sudo_ok(&["sh", "-c", &script]);
}

/// Every marker in a chroot, as the build would read them.
fn read_gen(chroot: &Path) -> Vec<String> {
    (0..8)
        .map(|i| {
            std::fs::read_to_string(chroot.join(format!("gen-{i}")))
                .unwrap()
                .trim()
                .to_string()
        })
        .collect()
}

/// Write `bytes` of incompressible data as root; the base and its candidates
/// are root-owned, so the worker cannot write them itself.
fn write_root(path: &Path, bytes: u64) {
    sudo_ok(&[
        "sh",
        "-c",
        &format!(
            "head -c {bytes} /dev/urandom > '{}' && sync -f '{0}'",
            path.display()
        ),
    ]);
}

#[tokio::test]
async fn a_build_is_held_to_its_limit_and_the_pool_to_its_total() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("limits");
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();

    let error = pool.lease(1, Some(64 * MIB)).await.unwrap_err();
    assert!(format!("{error:#}").contains("no base chroot"), "{error:#}");

    make_base(&pool, 100).await;
    sync(&pool);
    let total_before = pool.total_usage().unwrap().used;
    assert!(
        total_before >= 100 * MIB,
        "the base counts against the total"
    );

    // One build, limited to 64M.
    let build = pool.lease(1, Some(64 * MIB)).await.unwrap();
    assert_eq!(build.label(), "job-1");
    assert!(
        build.chroot().join("usr/blob").exists(),
        "a snapshot of the base"
    );
    sync(&pool);
    let fresh = pool.usage(build.group()).unwrap();
    assert!(
        fresh.used < MIB,
        "a fresh snapshot is charged ~nothing: {fresh:?}"
    );
    assert_eq!(fresh.limit, Some(64 * MIB));

    let refused = fallocate(&build.data().join("big"), 256 * MIB);
    assert!(
        refused.as_deref().is_some_and(|e| e.contains("quota")),
        "fallocate past the limit: {refused:?}"
    );
    let refused = write(&build.data().join("big"), 100 * MIB);
    assert!(refused.is_some(), "a write past the limit must fail");
    sync(&pool);
    assert!(pool.usage(build.group()).unwrap().at_limit(2 * MIB));

    // A second, unlimited build still stops at the pool's total.
    let other = pool.lease(2, None).await.unwrap();
    let refused = write(&other.data().join("big"), 700 * MIB);
    assert!(refused.is_some(), "the total must bind an unlimited build");
    sync(&pool);
    let total = pool.total_usage().unwrap();
    assert!(total.at_limit(4 * MIB), "{total:?}");

    // Releasing gives the space back, once the cleaner has run.
    pool.release(other).await;
    pool.release(build).await;
    assert!(!pool.path().join("job-1").exists());
    assert!(!pool.path().join("job-2.data").exists());
    sudo_ok(&["btrfs", "subvolume", "sync", pool.path().to_str().unwrap()]);
    sync(&pool);
    let after = pool.total_usage().unwrap().used;
    assert!(
        after < total_before + 4 * MIB,
        "the total drops back to about the base: {after} vs {total_before}"
    );

    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn a_sweep_clears_what_a_killed_worker_left() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("sweep");
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    make_base(&pool, 10).await;

    let running = pool.lease(7, Some(64 * MIB)).await.unwrap();
    let crashed = pool.lease(8, Some(64 * MIB)).await.unwrap();
    // A worker killed mid-build: the volumes are never released.
    std::mem::forget(crashed);

    let cleared = pool.sweep(&HashSet::from([7]), None).await;
    assert_eq!(cleared, 1);
    assert!(
        pool.path().join("job-7").exists(),
        "a running build is kept"
    );
    assert!(!pool.path().join("job-8").exists());
    assert!(!pool.path().join("job-8.data").exists());

    pool.release(running).await;
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn reopening_finds_the_same_pool() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("reopen");
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    make_base(&pool, 10).await;
    let marker = pool.path().join("kept");
    std::fs::write(&marker, "still here").unwrap();

    // Still mounted: a worker restart.
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    assert!(marker.exists());

    // Unmounted and detached: a reboot. No mkfs this time.
    pool.unmount().await.unwrap();
    let attached = sudo(&[
        "losetup",
        "-j",
        scratch.0.join("pool.img").to_str().unwrap(),
    ]);
    assert!(
        attached.stdout.is_empty(),
        "the loop device outlived the mount: {}",
        String::from_utf8_lossy(&attached.stdout)
    );
    let pool = Pool::open(config(&scratch, 800 * MIB)).await.unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "still here");
    assert_eq!(pool.total_usage().unwrap().limit, Some(800 * MIB));
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn a_device_with_someone_elses_filesystem_is_never_formatted() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("foreign");
    let image = scratch.0.join("foreign.img");
    std::fs::File::create(&image)
        .unwrap()
        .set_len(256 * MIB)
        .unwrap();
    let device =
        String::from_utf8(sudo(&["losetup", "--find", "--show", image.to_str().unwrap()]).stdout)
            .unwrap()
            .trim()
            .to_string();
    sudo_ok(&["mkfs.ext4", "-q", &device]);

    let error = Pool::open(PoolConfig {
        backing: Backing::Device(PathBuf::from(&device)),
        mountpoint: scratch.0.join("pool"),
        total: 128 * MIB,
        owner: owner(),
    })
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("refusing"), "{error:#}");
    let still =
        String::from_utf8(sudo(&["blkid", "-o", "value", "-s", "TYPE", &device]).stdout).unwrap();
    assert_eq!(still.trim(), "ext4", "the device must be left as it was");
}

#[tokio::test]
async fn the_image_follows_the_total_both_ways() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("resize");
    let image = scratch.0.join("pool.img");
    let len = || std::fs::metadata(&image).unwrap().len();
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    make_base(&pool, 50).await;
    let small = len();
    assert_eq!(small, aurcache_chroot::pool::size_for(600 * MIB));

    assert_eq!(pool.set_total(4096 * MIB).await.unwrap(), Resize::Fits);
    assert_eq!(len(), aurcache_chroot::pool::size_for(4096 * MIB), "grown");

    assert_eq!(pool.set_total(600 * MIB).await.unwrap(), Resize::Fits);
    assert_eq!(len(), small, "shrunk back: the base fits");
    assert_eq!(pool.total_usage().unwrap().limit, Some(600 * MIB));
    assert!(pool.root().join("usr/blob").exists(), "the data survived");
    assert!(
        pool.host_free().is_some(),
        "a sparse image reports the host's room"
    );
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn a_cache_subvolume_counts_against_the_total() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("cache");
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    for bad in ["", "root", "job-3", "../escape", "a/b", "Cache"] {
        assert!(
            pool.ensure_subvolume(bad, owner(), 0o2775).await.is_err(),
            "{bad:?} must be refused"
        );
    }
    let cache = pool
        .ensure_subvolume("cache", owner(), 0o2775)
        .await
        .unwrap();
    let again = pool
        .ensure_subvolume("cache", owner(), 0o2775)
        .await
        .unwrap();
    assert_eq!(cache, again, "made once, found after");
    let before = pool.total_usage().unwrap().used;
    assert!(
        write(&cache.join("src"), 64 * MIB).is_none(),
        "the owner writes there"
    );
    sync(&pool);
    assert!(
        pool.total_usage().unwrap().used >= before + 60 * MIB,
        "the cache counts against the total"
    );
    let refused = write(&cache.join("more"), 700 * MIB);
    assert!(refused.is_some(), "and is bounded by it");
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn a_released_builds_group_is_cleared_by_a_later_lease() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("tidy");
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    make_base(&pool, 10).await;

    let first = pool.lease(1, Some(64 * MIB)).await.unwrap();
    let group = first.group();
    assert!(write(&first.data().join("x"), 8 * MIB).is_none());
    pool.release(first).await;
    // What production sees: the release returns before btrfs's cleaner has
    // freed the subvolumes, so the group may still be there. Once the
    // cleaner is done, the next lease clears it.
    sudo_ok(&["btrfs", "subvolume", "sync", pool.path().to_str().unwrap()]);
    let second = pool.lease(2, Some(64 * MIB)).await.unwrap();
    assert!(
        pool.usage(group).is_none(),
        "the first build's group outlived the next lease"
    );
    pool.release(second).await;
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn a_pool_too_full_to_shrink_is_made_again_at_the_new_size() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("recreate");
    let image = scratch.0.join("pool.img");
    let pool = Pool::open(config(&scratch, 4096 * MIB)).await.unwrap();
    let cache = pool
        .ensure_subvolume("cache", owner(), 0o2775)
        .await
        .unwrap();
    assert!(write(&cache.join("big"), 3072 * MIB).is_none());
    sync(&pool);

    // 3 GiB held, and the new total asks for 600 MiB plus the 2 GiB margin.
    let resize = pool.set_total(600 * MIB).await.unwrap();
    assert!(matches!(resize, Resize::TooFull { .. }), "{resize:?}");
    assert!(pool.oversized().is_some());
    assert_eq!(
        pool.total_usage().unwrap().limit,
        Some(600 * MIB),
        "the lower total binds regardless"
    );

    pool.destroy().await.unwrap();
    assert!(!image.exists());
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    assert_eq!(
        std::fs::metadata(&image).unwrap().len(),
        aurcache_chroot::pool::size_for(600 * MIB),
        "made again at the new size"
    );
    assert!(!pool.path().join("cache").exists(), "and empty");
    assert!(pool.oversized().is_none());
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn cache_entries_are_subvolumes_measured_and_removed_by_btrfs() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("volumes");
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    pool.ensure_subvolume("cache", owner(), 0o2775)
        .await
        .unwrap();
    let volumes = pool.cache_volumes("cache");
    let entry = volumes.root().join("srcdest/hello");

    let (v, e) = (volumes.clone(), entry.clone());
    tokio::task::spawn_blocking(move || {
        v.ensure(&e, owner(), 0o2775).unwrap();
        v.ensure(&e, owner(), 0o2775).unwrap();
    })
    .await
    .unwrap();
    assert!(volumes.is_volume(&entry), "made a subvolume");
    assert!(
        !volumes.is_volume(&volumes.root().join("srcdest")),
        "its parent stays a directory"
    );

    let before = pool.total_usage().unwrap().used;
    assert!(
        write(&entry.join("tarball"), 32 * MIB).is_none(),
        "the owner writes there"
    );
    sync(&pool);
    let used = volumes.usage(&entry).unwrap();
    assert!(used >= 30 * MIB, "measured by its own group: {used}");
    assert!(
        pool.total_usage().unwrap().used >= before + 30 * MIB,
        "and counted under the total"
    );

    // A plain directory from before entries were subvolumes: not measured by
    // a group, and never deleted as a subvolume.
    let legacy = volumes.root().join("srcdest/legacy");
    std::fs::create_dir_all(&legacy).unwrap();
    assert_eq!(volumes.usage(&legacy), None);
    assert!(volumes.remove(&legacy).is_err());
    assert!(
        volumes.remove(&pool.root()).is_err(),
        "nothing outside the cache"
    );

    let started = std::time::Instant::now();
    volumes.remove(&entry).unwrap();
    assert!(!entry.exists());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    pool.unmount().await.unwrap();
}

/// Bytes the pool's filesystem uses of its device.
fn fs_size(pool: &Pool) -> u64 {
    let out = sudo(&[
        "btrfs",
        "filesystem",
        "show",
        "--raw",
        pool.path().to_str().unwrap(),
    ]);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.first() == Some(&"devid")).then(|| f[3].parse().unwrap())
        })
        .unwrap()
}

#[tokio::test]
async fn a_device_pool_is_formatted_built_on_and_its_filesystem_fitted_to_the_total() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("device");
    let backing = scratch.0.join("disk.img");
    std::fs::File::create(&backing)
        .unwrap()
        .set_len(4096 * MIB)
        .unwrap();
    let device =
        String::from_utf8(sudo(&["losetup", "--find", "--show", backing.to_str().unwrap()]).stdout)
            .unwrap()
            .trim()
            .to_string();
    let config = |total| PoolConfig {
        backing: Backing::Device(PathBuf::from(&device)),
        mountpoint: scratch.0.join("pool"),
        total,
        owner: owner(),
    };

    // A blank device is formatted, and its filesystem fitted to the total.
    let pool = Pool::open(config(600 * MIB)).await.unwrap();
    assert_eq!(fs_size(&pool), aurcache_chroot::pool::size_for(600 * MIB));

    // Builds on it like on any pool.
    make_base(&pool, 10).await;
    let build = pool.lease(1, Some(64 * MIB)).await.unwrap();
    assert!(write(&build.data().join("x"), 8 * MIB).is_none());
    pool.release(build).await;

    // More than the device holds: grown to the device, and no further.
    assert_eq!(pool.set_total(4096 * MIB).await.unwrap(), Resize::Fits);
    assert_eq!(fs_size(&pool), 4096 * MIB);

    // Less than it holds: the filesystem keeps its size, and says so.
    sudo_ok(&[
        "sh",
        "-c",
        &format!(
            "head -c 2600M /dev/urandom > {}",
            scratch.0.join("pool/root/usr/fill").display()
        ),
    ]);
    sync(&pool);
    let resize = pool.set_total(64 * MIB).await.unwrap();
    assert!(matches!(resize, Resize::Kept { .. }), "{resize:?}");
    assert!(pool.oversized().is_none(), "a device is never made again");

    pool.unmount().await.unwrap();
    sudo_ok(&["losetup", "-d", &device]);
}

#[tokio::test]
async fn a_builds_usage_is_measured_part_by_part() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("usage");
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    make_base(&pool, 20).await;
    let build = pool.lease(1, Some(256 * MIB)).await.unwrap();
    assert!(write(&build.data().join("pkg.tar.zst"), 16 * MIB).is_none());
    sudo_ok(&[
        "sh",
        "-c",
        &format!(
            "head -c 8M /dev/urandom > {}",
            build.chroot().join("usr/dep").display()
        ),
    ]);

    // No sync by the caller: build_usage commits before it reads.
    let usage = pool.build_usage(&build).await;
    let chroot = usage.chroot.unwrap();
    let data = usage.data.unwrap();
    assert!(
        (7 * MIB..12 * MIB).contains(&chroot),
        "the chroot counts its own writes, not the 20M base it shares: {chroot}"
    );
    assert!((15 * MIB..20 * MIB).contains(&data), "{data}");
    pool.release(build).await;
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn the_exchange_swaps_the_base_atomically() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("exchange");
    let pool = Pool::open(config(&scratch, 2048 * MIB)).await.unwrap();
    make_base(&pool, 10).await;
    write_gen(&pool.root(), 0);

    // Sequential rounds: each lease sees exactly the generation swapped in.
    // A fresh id per round, as served builds have: a released id's group
    // lingers until the cleaner frees it, and is not leased again meanwhile.
    for generation in 1..=5u64 {
        pool.snapshot_next().await.unwrap();
        write_gen(&pool.next_root(), generation);
        pool.exchange_root().unwrap();
        pool.retire_prev().await.unwrap();
        let build = pool.lease(50 + generation as i32, None).await.unwrap();
        let seen = read_gen(build.chroot());
        assert!(
            seen.iter().all(|g| g == &generation.to_string()),
            "torn snapshot: {seen:?}"
        );
        pool.release(build).await;
    }

    // Concurrent: leases in a loop while exchanges land. Every snapshot is
    // all one generation -- never a mix -- and no lease fails on the swap.
    let pool = std::sync::Arc::new(pool);
    let leasers: Vec<_> = (0..4)
        .map(|t| {
            let pool = std::sync::Arc::clone(&pool);
            tokio::spawn(async move {
                let mut seen = Vec::new();
                for i in 0..10 {
                    let build = pool.lease(100 + t * 10 + i, None).await.unwrap();
                    seen.push(read_gen(build.chroot()));
                    pool.release(build).await;
                }
                seen
            })
        })
        .collect();
    for generation in 6..=15u64 {
        pool.snapshot_next().await.unwrap();
        write_gen(&pool.next_root(), generation);
        pool.exchange_root().unwrap();
        pool.retire_prev().await.unwrap();
    }
    for leaser in leasers {
        for seen in leaser.await.unwrap() {
            assert!(
                seen.iter().all(|g| g == &seen[0]),
                "torn snapshot: {seen:?}"
            );
            let generation: u64 = seen[0].parse().unwrap();
            assert!(
                generation <= 15,
                "a generation from the future: {generation}"
            );
        }
    }
    let pool = std::sync::Arc::try_unwrap(pool).unwrap();
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn a_discarded_candidate_leaves_the_base_untouched() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("discard-next");
    let pool = Pool::open(config(&scratch, 1024 * MIB)).await.unwrap();
    make_base(&pool, 10).await;
    write_gen(&pool.root(), 0);
    let blob = pool.root().join("usr/blob");
    let before = std::fs::read(&blob).unwrap();

    // A failed upgrade, every way one can fail: deleted, rewritten, added.
    pool.snapshot_next().await.unwrap();
    let next = pool.next_root();
    sudo_ok(&["rm", next.join("usr/blob").to_str().unwrap()]);
    write_gen(&next, 999);
    sudo_ok(&["touch", next.join("garbage").to_str().unwrap()]);
    pool.discard_next().await;

    assert!(!pool.next_root().exists(), "the candidate goes");
    assert_eq!(std::fs::read(&blob).unwrap(), before, "byte-for-byte");
    assert!(
        read_gen(&pool.root()).iter().all(|g| g == "0"),
        "no marker moved"
    );
    assert!(!pool.root().join("garbage").exists());

    // And a refused exchange is nothing at all: no candidate, no change.
    let refused = pool.exchange_root().unwrap_err();
    assert!(format!("{refused:#}").contains("exchanging"), "{refused:#}");
    assert_eq!(std::fs::read(&blob).unwrap(), before);
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn refresh_recovery_finishes_both_crash_states() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("recover");
    let pool = Pool::open(config(&scratch, 1024 * MIB)).await.unwrap();
    make_base(&pool, 10).await;
    write_gen(&pool.root(), 0);

    // Died before the exchange: an unfinished candidate goes, the base stays.
    pool.snapshot_next().await.unwrap();
    write_gen(&pool.next_root(), 1);
    pool.recover_refresh().await.unwrap();
    assert!(!pool.next_root().exists());
    assert!(read_gen(&pool.root()).iter().all(|g| g == "0"));

    // Died between the exchange and the retire: `root.next` is the previous
    // base, and becomes `root.prev`.
    pool.snapshot_next().await.unwrap();
    write_gen(&pool.next_root(), 2);
    pool.exchange_root().unwrap();
    pool.recover_refresh().await.unwrap();
    assert!(!pool.next_root().exists());
    assert!(
        read_gen(&pool.root()).iter().all(|g| g == "2"),
        "root is the new base"
    );
    assert!(
        read_gen(&pool.prev_root()).iter().all(|g| g == "0"),
        "prev is the old one"
    );

    // No base at all: a stray candidate goes, and the caller remakes the base
    // from nothing.
    pool.discard_root().await.unwrap();
    sudo_ok(&[
        "btrfs",
        "subvolume",
        "create",
        pool.next_root().to_str().unwrap(),
    ]);
    pool.recover_refresh().await.unwrap();
    assert!(!pool.next_root().exists());
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn the_total_survives_a_swap_and_clear_stale_keeps_its_holders() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("swap-total");
    let pool = Pool::open(config(&scratch, 2048 * MIB)).await.unwrap();
    make_base(&pool, 50).await;
    sync(&pool);
    let before = pool.total_usage().unwrap().used;

    pool.snapshot_next().await.unwrap();
    write_root(&pool.next_root().join("added"), 10 * MIB);
    pool.exchange_root().unwrap();
    pool.retire_prev().await.unwrap();
    sync(&pool);
    let after = pool.total_usage().unwrap().used;
    assert!(
        (before + 8 * MIB..before + 15 * MIB).contains(&after),
        "the total counts the refresh's writes once: {before} -> {after}"
    );

    // A second cycle deletes the first retired base while the live bases
    // still share its extents: its group stays as a charged space holder, and
    // `clear-stale` leaves it -- only empty groups go.
    pool.snapshot_next().await.unwrap();
    pool.exchange_root().unwrap();
    pool.retire_prev().await.unwrap();
    sudo_ok(&[
        "btrfs",
        "qgroup",
        "clear-stale",
        pool.path().to_str().unwrap(),
    ]);
    sync(&pool);
    let cleared = pool.total_usage().unwrap().used;
    assert!(
        cleared.abs_diff(after) < 5 * MIB,
        "clearing stale groups keeps charged holders: {after} -> {cleared}"
    );
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn a_kept_build_keeps_its_group_limit_and_locks_down() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("keep");
    let pool = Pool::open(config(&scratch, 1024 * MIB)).await.unwrap();
    make_base(&pool, 10).await;

    let build = pool.lease(7, Some(64 * MIB)).await.unwrap();
    assert!(write(&build.data().join("output"), 5 * MIB).is_none());
    let group = build.group();
    let kept = pool
        .keep_build(build, Duration::from_secs(3600), None)
        .await
        .unwrap();

    // As root: the keep locked the tops down to the worker, and this test
    // is not it.
    sudo_ok(&["test", "-e", kept.path.join("usr/blob").to_str().unwrap()]);
    sudo_ok(&[
        "test",
        "-e",
        pool.path().join("kept-7.data/output").to_str().unwrap(),
    ]);
    assert!(!pool.path().join("job-7").exists());
    assert!(!pool.path().join("job-7.data").exists());
    let usage = pool.usage(group).unwrap();
    assert_eq!(usage.limit, Some(64 * MIB), "a kept build can never grow");
    assert!(usage.used >= 5 * MIB, "and it still counts: {usage:?}");
    let until = kept.until.duration_since(SystemTime::now()).unwrap();
    assert!(
        until > Duration::from_secs(3500) && until <= Duration::from_secs(3600),
        "the keep runs about an hour: {until:?}"
    );

    // Locked down: readable only by the worker, and no sockets left behind
    // by the agent bind.
    use std::os::unix::fs::PermissionsExt;
    for name in ["kept-7", "kept-7.data"] {
        let mode = std::fs::metadata(pool.path().join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{name}");
    }
    // As root, for the same reason: a directory this test cannot enter
    // would read as socket-free either way.
    let found = sudo(&[
        "find",
        pool.path().join("kept-7").to_str().unwrap(),
        pool.path().join("kept-7.data").to_str().unwrap(),
        "-type",
        "s",
    ]);
    assert!(
        found.stdout.is_empty(),
        "sockets under a kept build: {}",
        String::from_utf8_lossy(&found.stdout)
    );
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn a_keep_carries_its_build_tree() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("keep-tree");
    let pool = Pool::open(config(&scratch, 1024 * MIB)).await.unwrap();
    make_base(&pool, 10).await;

    // The tree as the worker keeps it: a subvolume inside the cache
    // subvolume -- here root-owned, as a build that became root can leave
    // it. Moving it crosses a subvolume boundary, which btrfs allows for a
    // subvolume and refuses for anything else.
    let cache = pool.path().join("tree-cache");
    sudo_ok(&["btrfs", "subvolume", "create", cache.to_str().unwrap()]);
    let tree = cache.join("hello");
    sudo_ok(&["btrfs", "subvolume", "create", tree.to_str().unwrap()]);
    sudo_ok(&[
        "sh",
        "-c",
        &format!("echo built > '{}'/output", tree.display()),
    ]);
    // And a plain-directory tree, from before trees were subvolumes.
    let plain = cache.join("plain");
    sudo_ok(&["mkdir", plain.to_str().unwrap()]);

    let build = pool.lease(7, None).await.unwrap();
    let kept = pool
        .keep_build(build, Duration::from_secs(3600), Some(&tree))
        .await
        .unwrap();
    let moved = pool.path().join("kept-7.build");
    assert_eq!(kept.tree.as_deref(), Some(moved.as_path()));
    assert!(!tree.exists(), "the package finds no tree");

    // Locked down with the rest of the keep.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&moved).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "kept-7.build");
    let output = sudo(&["cat", moved.join("output").to_str().unwrap()]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "built",
        "the tree lands beside the keep"
    );

    // A plain directory cannot leave the cache subvolume: it stays, and is
    // never copied.
    let build = pool.lease(8, None).await.unwrap();
    let kept = pool
        .keep_build(build, Duration::from_secs(3600), Some(&plain))
        .await
        .unwrap();
    assert_eq!(kept.tree, None);
    assert!(plain.exists(), "left for the caller to discard");
    assert!(
        !pool.path().join("kept-8.build").exists(),
        "and never copied"
    );

    // A path outside the pool is refused, and the build's volumes go.
    let build = pool.lease(9, None).await.unwrap();
    assert!(
        pool.keep_build(build, Duration::from_secs(3600), Some(Path::new("/etc")))
            .await
            .is_err()
    );
    assert!(!pool.path().join("job-9").exists());
    assert!(!pool.path().join("kept-9").exists());

    // The keep goes away with its tree.
    pool.sweep(&HashSet::new(), None).await;
    assert!(!pool.path().join("kept-7").exists());
    assert!(!pool.path().join("kept-7.data").exists());
    assert!(!moved.exists());
    sudo_ok(&[
        "btrfs",
        "subvolume",
        "delete",
        "--recursive",
        cache.to_str().unwrap(),
    ]);
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn the_sweep_keeps_a_fresh_failure_and_deletes_an_expired_one() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("expire");
    let pool = Pool::open(config(&scratch, 1024 * MIB)).await.unwrap();
    make_base(&pool, 10).await;

    let fresh = pool.lease(7, None).await.unwrap();
    pool.keep_build(fresh, Duration::from_secs(3600), None)
        .await
        .unwrap();
    let stale = pool.lease(8, None).await.unwrap();
    pool.keep_build(stale, Duration::from_secs(3600), None)
        .await
        .unwrap();
    // Expired an hour ago: the keep runs from the chroot's mtime.
    sudo_ok(&[
        "touch",
        "-d",
        "2 hours ago",
        pool.path().join("kept-8").to_str().unwrap(),
    ]);

    let cleared = pool
        .sweep(&HashSet::new(), Some(Duration::from_secs(3600)))
        .await;
    assert_eq!(cleared, 1);
    assert!(pool.path().join("kept-7").exists(), "a fresh keep stays");
    assert!(!pool.path().join("kept-8").exists());
    assert!(!pool.path().join("kept-8.data").exists());
    // The sweep ran the group tidy, which leaves a kept failure's group
    // alone: destroying it would be refused at every lease, forever.
    assert!(pool.usage(QgroupId::build(7)).is_some());

    // Keeping off deletes every kept failure. At least one: the sweep also
    // counts the lingering group of the keep it just removed, when the
    // cleaner has not freed it yet.
    assert!(pool.sweep(&HashSet::new(), None).await >= 1);
    assert!(!pool.path().join("kept-7").exists());
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn room_making_takes_kept_failures_before_the_previous_base() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("reclaim");
    let pool = Pool::open(config(&scratch, 400 * MIB)).await.unwrap();
    make_base(&pool, 50).await;

    // A kept failure holding ~100M, and a retired base.
    let build = pool.lease(7, None).await.unwrap();
    assert!(write(&build.data().join("output"), 100 * MIB).is_none());
    pool.keep_build(build, Duration::from_secs(3600), None)
        .await
        .unwrap();
    pool.snapshot_next().await.unwrap();
    pool.exchange_root().unwrap();
    pool.retire_prev().await.unwrap();
    sync(&pool);
    let used = pool.total_usage().unwrap().used;

    // Room for this fits once the kept failure goes, with the retired base to
    // spare: the figures have tens of megabytes of slack either way.
    let need = (400 * MIB).saturating_sub(used).saturating_add(90 * MIB);
    pool.reclaim_room(need).await;
    assert!(
        !pool.path().join("kept-7").exists(),
        "the kept failure goes first"
    );
    assert!(
        pool.prev_root().exists(),
        "but nothing is taken past what fits"
    );

    // Room for the whole total fits never: the retired base goes too, and
    // then there is nothing left to take.
    pool.reclaim_room(400 * MIB).await;
    assert!(!pool.prev_root().exists());
    pool.unmount().await.unwrap();
}
