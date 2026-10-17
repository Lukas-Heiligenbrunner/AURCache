//! The worker's caches in the storage pool: each package's sources and each
//! kept build tree is a subvolume, measured by its quota group and removed by
//! deleting it.
//!
//! Needs root through `sudo -n` and makes real mounts, so it only runs when
//! asked to: `AURCACHE_POOL_TESTS=1 cargo test -p aurcache-worker --test
//! cache_volumes`. Its image goes under Cargo's target directory, not `/tmp`.

use aurcache_chroot::{Backing, Owner, PoolConfig};
use aurcache_worker::cache::Cache;
use aurcache_worker::chroots::Chroots;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const MIB: u64 = 1 << 20;

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

fn write(path: &Path, megabytes: u64) {
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "head -c {megabytes}M /dev/urandom > '{}' && sync -f '{0}'",
            path.display()
        ))
        .status()
        .unwrap();
    assert!(status.success(), "writing {}", path.display());
}

#[tokio::test]
async fn cache_entries_are_subvolumes_that_eviction_deletes() {
    if std::env::var_os("AURCACHE_POOL_TESTS").is_none() {
        eprintln!("skipping: set AURCACHE_POOL_TESTS=1 to run pool tests (needs sudo)");
        return;
    }
    let scratch = Scratch(Path::new(env!("CARGO_TARGET_TMPDIR")).join("worker-cache-volumes"));
    drop(Scratch(scratch.0.clone()));
    std::fs::create_dir_all(&scratch.0).unwrap();
    let mountpoint = scratch.0.join("pool");
    let chroots = Chroots::new(
        PoolConfig {
            backing: Backing::Image {
                path: scratch.0.join("pool.img"),
                reserve: false,
            },
            mountpoint: mountpoint.clone(),
            total: 2048 * MIB,
            owner: Owner::current(),
        },
        Duration::from_secs(3600),
        None,
        Owner::current(),
    );
    assert!(chroots.ready_for_work(64 * MIB, 0).await);
    let volumes = chroots.cache_volumes().await.expect("the pool is open");

    // A 1-byte source budget, so anything cached is over it; no TTL.
    let cache =
        Cache::new(&mountpoint.join("cache"), 1, 0, 0, 0).with_volumes(Some(volumes.clone()));
    let (c, v) = (cache.clone(), volumes.clone());
    tokio::task::spawn_blocking(move || {
        let srcdest = c.srcdest("hello").expect("a source cache");
        assert!(v.is_volume(&srcdest), "the sources are a subvolume");
        write(&srcdest.join("hello.tar.gz"), 16);
        let tree = c.builddir_tree("x86_64", "hello").expect("a build tree");
        assert!(v.is_volume(&tree), "the kept tree is a subvolume");
        write(&tree.join("object.o"), 16);
        let other = c.builddir_tree("x86_64", "world").expect("another tree");
        write(&other.join("object.o"), 16);

        // Measured by their groups, not walked.
        assert!(v.usage(&srcdest).unwrap() >= 15 * MIB);

        // Over the source budget: evicted, instantly, unless in use.
        assert!(c.evict(&["hello".to_string()]).is_empty(), "in use: kept");
        assert_eq!(c.evict(&[]), ["hello"]);
        assert!(!srcdest.exists());

        // Over a 1-byte tree budget: the tree not in use goes.
        c.reclaim_builddirs("x86_64", &["hello".to_string()], 1, 0);
        assert!(tree.exists(), "a tree in use is kept");
        assert!(!other.exists(), "the idle tree is reclaimed");
    })
    .await
    .unwrap();
}
