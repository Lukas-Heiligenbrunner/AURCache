//! Lowering `WORKER_DISK_MAX` below what an image pool holds: the worker
//! stops taking builds, lets the running one finish, and makes the pool again
//! at the new size.
//!
//! Needs root through `sudo -n` and makes real mounts, so it only runs when
//! asked to: `AURCACHE_POOL_TESTS=1 cargo test -p aurcache-worker --test
//! pool_drain`. Its image goes under Cargo's target directory, not `/tmp`.

use aurcache_chroot::{Backing, Owner, PoolConfig};
use aurcache_worker::chroots::Chroots;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const MIB: u64 = 1 << 20;

fn sudo(args: &[&str]) {
    let out = Command::new("sudo").arg("-n").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "sudo {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Removed, with whatever is mounted in it, when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let dir = self.0.display();
        let script = format!(
            r#"for m in $(findmnt -rn -o TARGET | grep "^{dir}" | sort -r); do umount -l "$m"; done
               for f in {dir}/*.img; do [ -e "$f" ] && losetup -j "$f" | cut -d: -f1 | xargs -r -n1 losetup -d; done
               rm -rf "{dir}""#
        );
        let _ = Command::new("sudo")
            .args(["-n", "sh", "-c", &script])
            .status();
    }
}

fn owner() -> Owner {
    Owner::current()
}

#[tokio::test]
async fn an_oversized_pool_is_made_again_once_its_builds_finish() {
    if std::env::var_os("AURCACHE_POOL_TESTS").is_none() {
        eprintln!("skipping: set AURCACHE_POOL_TESTS=1 to run pool tests (needs sudo)");
        return;
    }
    let scratch = Scratch(Path::new(env!("CARGO_TARGET_TMPDIR")).join("worker-pool-drain"));
    drop(Scratch(scratch.0.clone()));
    std::fs::create_dir_all(&scratch.0).unwrap();
    let image = scratch.0.join("pool.img");
    let mountpoint = scratch.0.join("pool");
    let chroots = Chroots::new(
        PoolConfig {
            backing: Backing::Image {
                path: image.clone(),
                reserve: false,
            },
            mountpoint: mountpoint.clone(),
            total: 4096 * MIB,
            owner: owner(),
        },
        Duration::from_secs(3600),
        None,
        owner(),
    );
    assert!(chroots.ready_for_work(64 * MIB, 0).await);

    // A stand-in base chroot, as `mkarchroot` would leave one.
    let root = mountpoint.join("root");
    sudo(&["btrfs", "subvolume", "create", root.to_str().unwrap()]);
    sudo(&["mkdir", root.join("usr").to_str().unwrap()]);

    // A build is running, and the cache holds more than a smaller pool could.
    let lease = chroots.acquire(7, Some(512 * MIB)).await.unwrap();
    let filler = mountpoint.join("cache/filler");
    let wrote = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "head -c 3G /dev/urandom > '{}' && sync -f '{0}'",
            filler.display()
        ))
        .status()
        .unwrap();
    assert!(wrote.success());

    chroots.set_total(600 * MIB).await.unwrap();
    assert!(
        !chroots.ready_for_work(64 * MIB, 0).await,
        "a worker whose pool must be made again takes no new build"
    );
    assert!(filler.exists(), "nothing is thrown away while a build runs");

    chroots.release(lease).await;
    assert!(
        !chroots.ready_for_work(64 * MIB, 1).await,
        "a job claimed but not yet leased still keeps the pool"
    );
    assert!(
        filler.exists(),
        "nothing is thrown away while a job sets up"
    );
    assert!(
        chroots.ready_for_work(64 * MIB, 0).await,
        "with nothing running, the pool is made again and work resumes"
    );
    assert!(!filler.exists(), "the new pool starts empty");
    assert!(
        mountpoint.join("cache").is_dir(),
        "with its cache subvolume"
    );
    assert_eq!(
        std::fs::metadata(&image).unwrap().len(),
        aurcache_chroot::pool::size_for(600 * MIB),
        "at the new size"
    );
}
