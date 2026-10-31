//! The pool: a btrfs filesystem the worker owns, mounted in one place, with
//! simple quotas enabled and a total over everything in it.
//!
//! It is backed by whatever the operator can give it ([`Backing`]): an image
//! file (the default, needing nothing from the host), a block device such as a
//! zvol, or an existing dedicated btrfs mount. Only the image needs a loop
//! device.

use crate::cmd::{privileged, query};
use crate::qgroup::{QgroupId, Qgroups, TOTAL, Usage};
use crate::volumes::CacheVolumes;
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The label `mkfs.btrfs` gives a pool, and what an existing device must carry
/// before it is used: a device whose filesystem this worker did not create is
/// never formatted and never adopted.
pub const LABEL: &str = "aurcache-pool";

/// Kept free under the pool's size, whatever the total: a btrfs filesystem
/// that is really full is the one state where deleting from it can fail, so
/// the total binds before the filesystem fills.
pub const MIN_MARGIN: u64 = 2 << 30;

/// The base chroot's subvolume, inside the pool. `makechrootpkg -r <pool>`
/// expects it under this name.
pub const ROOT: &str = "root";
/// A refresh's candidate base: snapshotted from [`ROOT`], upgraded and
/// checked, then exchanged into its place.
pub const ROOT_NEXT: &str = "root.next";
/// The previous base, kept as the rollback point until the next refresh
/// replaces it.
pub const ROOT_PREV: &str = "root.prev";
/// devtools' lock beside the base, unused since the refresh stopped upgrading
/// it in place; deleted by the sweep.
const ROOT_LOCK: &str = "root.lock";
/// The file the refresh's cross-process exclusion is a `flock` on. A plain
/// file at the pool's top, worker-owned; the kernel drops the lock when its
/// holder dies, so a crash never leaves one behind.
const REFRESH_LOCK: &str = "refresh.lock";
/// A kept failure's chroot: `kept-<id>`, renamed from `job-<id>` when its
/// build failed. See [`Pool::keep_build`].
const KEPT_PREFIX: &str = "kept-";

/// Mount options every pool gets. `-m single` at mkfs time, and these at
/// mount: the host has its own redundancy, atime updates are writes nobody
/// reads, and compressing inside the pool means fewer bytes reach the host.
/// `commit=120` halves the flushes a nested filesystem sends its host.
const MOUNT_OPTIONS: &str = "noatime,compress=zstd:1,commit=120";

/// What backs the pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backing {
    /// An image file, loop-mounted. Sparse unless `reserve`, in which case it
    /// is allocated in full and mounted without `discard`, so its space stays
    /// the pool's.
    Image { path: PathBuf, reserve: bool },
    /// A block device -- a zvol, a partition -- formatted the first time.
    Device(PathBuf),
    /// An existing btrfs filesystem, mounted at the pool's mount point and
    /// dedicated to it: quotas apply filesystem-wide.
    Mount,
}

/// Who owns something the pool makes for the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

impl Owner {
    /// The user this process runs as.
    #[must_use]
    pub fn current() -> Self {
        // SAFETY: neither call has preconditions or can fail.
        unsafe {
            Self {
                uid: libc::getuid(),
                gid: libc::getgid(),
            }
        }
    }

    /// A user's uid and primary gid, by name.
    ///
    /// Through the passwd *database*, not `/etc/passwd`: the file is only one
    /// source, and accounts that come from LDAP, sssd or systemd-homed have no
    /// line in it. `None` when there is no such user.
    #[must_use]
    pub fn of_user(name: &str) -> Option<Self> {
        let name = std::ffi::CString::new(name).ok()?;
        // SAFETY: `passwd` is plain data, and all-zero is a valid value of it.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        // Generous enough for any real entry; `getpwnam_r` reports ERANGE
        // rather than overrunning it, and that reads as "no such user".
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and outlives the call, `pwd` and
        // `found` are valid out-parameters, and `buf` is the scratch space the
        // call is told the length of. The `_r` form keeps its result in ours
        // rather than in static storage, so concurrent callers do not race.
        let rc = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                &raw mut pwd,
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut found,
            )
        };
        (rc == 0 && !found.is_null()).then_some(Self {
            uid: pwd.pw_uid,
            gid: pwd.pw_gid,
        })
    }

    /// `uid:gid`, as `chown` takes it.
    pub(crate) fn chown_spec(self) -> String {
        format!("{}:{}", self.uid, self.gid)
    }
}

/// How to open a pool.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub backing: Backing,
    /// Where the pool is mounted: made here for an image or a device, and
    /// already there for [`Backing::Mount`].
    pub mountpoint: PathBuf,
    /// Everything the worker may store, in bytes (`WORKER_DISK_MAX`).
    pub total: u64,
    /// The worker's uid and gid: the owner of what it writes in the pool.
    pub owner: Owner,
}

/// The size an image or device needs for a total of `total`: the total plus
/// its margin.
#[must_use]
pub fn size_for(total: u64) -> u64 {
    total.saturating_add((total / 20).max(MIN_MARGIN))
}

/// An open, mounted pool.
#[derive(Debug)]
pub struct Pool {
    mountpoint: PathBuf,
    qgroups: Qgroups,
    owner: Owner,
    backing: Backing,
    /// The loop device, for an image.
    loop_device: Option<PathBuf>,
    /// Set when an image could not shrink to its total: the size it would
    /// have had. See [`Self::oversized`].
    oversized: std::sync::Mutex<Option<u64>>,
}

/// What [`Pool::set_total`] did to the pool's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Resize {
    /// The pool's size follows the total: it was grown or shrunk to it, or
    /// needed neither.
    Fits,
    /// A device's or a mount's filesystem holds more than fits in the size
    /// the total asks for, so it kept its size. The total still binds. The
    /// worker never empties those -- they are the operator's -- so this is
    /// for them to act on.
    Kept {
        /// The filesystem's size now.
        size: u64,
        /// The size the total asks for.
        wanted: u64,
    },
    /// An image holds more than fits in the size the total asks for, so it
    /// kept its size. The total still binds. Emptying the pool and making it
    /// again is the way down; see [`Pool::destroy`].
    TooFull {
        /// The image's size now.
        size: u64,
        /// The size the total asks for.
        wanted: u64,
    },
}

impl Pool {
    /// Mount the pool, creating it the first time, and make sure its quotas
    /// are on and its total is `config.total`.
    pub async fn open(config: PoolConfig) -> Result<Self> {
        let PoolConfig {
            backing,
            mountpoint,
            total,
            owner,
        } = config;
        let mut loop_device = None;
        match &backing {
            Backing::Mount => check_dedicated_mount(&mountpoint).await?,
            Backing::Image { path, reserve } => {
                if !is_mountpoint(&mountpoint) {
                    // Formatted and checked as a file: neither needs a loop
                    // device.
                    match prepare_image(path, size_for(total), *reserve).await? {
                        Image::Created => mkfs(path).await?,
                        Image::Existing => check_ours(path).await?,
                    }
                    ensure_free_loop_node().await?;
                    mount(path, &mountpoint, !reserve, true, owner).await?;
                }
                loop_device = attached_loop(path).await;
                if let Some(device) = &loop_device {
                    enable_direct_io(device).await;
                }
                advise_on_zfs(path);
            }
            Backing::Device(device) => {
                if !is_mountpoint(&mountpoint) {
                    check_device(device).await?;
                    match filesystem_type(device).await?.as_deref() {
                        None => mkfs(device).await?,
                        Some(_) => check_ours(device).await?,
                    }
                    mount(device, &mountpoint, true, false, owner).await?;
                }
            }
        }

        let fsid = fsid(&mountpoint).await?;
        let pool = Self {
            qgroups: Qgroups::for_fsid(&fsid),
            mountpoint,
            owner,
            backing,
            loop_device,
            oversized: std::sync::Mutex::new(None),
        };
        pool.enable_quotas().await?;
        // A lowered total the image cannot shrink to is remembered in
        // `oversized` for the caller; the pool opens either way.
        let _ = pool.set_total(total).await?;
        Ok(pool)
    }

    /// Where the pool is mounted: `makechrootpkg -r` takes this.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.mountpoint
    }

    /// The base chroot's path, whether or not it exists yet.
    #[must_use]
    pub fn root(&self) -> PathBuf {
        self.mountpoint.join(ROOT)
    }

    /// The refresh candidate's path, whether or not a refresh has made it.
    #[must_use]
    pub fn next_root(&self) -> PathBuf {
        self.mountpoint.join(ROOT_NEXT)
    }

    /// The previous base's path, whether or not a refresh has left one.
    #[must_use]
    pub fn prev_root(&self) -> PathBuf {
        self.mountpoint.join(ROOT_PREV)
    }

    /// The refresh's cross-process exclusion, taken without waiting: `None`
    /// when another process is refreshing now. Held until the returned file is
    /// dropped; the kernel drops it for a dead holder, so a crash never leaves
    /// one behind -- which is what lets recovery treat anything it finds as
    /// dead.
    #[must_use]
    pub fn try_refresh_lock(&self) -> Option<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.mountpoint.join(REFRESH_LOCK))
            .ok()?;
        file.try_lock().ok()?;
        Some(file)
    }

    /// Everything stored in the pool, against `WORKER_DISK_MAX`.
    #[must_use]
    pub fn total_usage(&self) -> Option<Usage> {
        self.qgroups.usage(TOTAL)
    }

    /// A qgroup's usage.
    #[must_use]
    pub fn usage(&self, group: QgroupId) -> Option<Usage> {
        self.qgroups.usage(group)
    }

    /// The size an image would have for its total, when it could not shrink
    /// to it: what it holds did not fit. `None` when the size follows the
    /// total.
    #[must_use]
    pub fn oversized(&self) -> Option<u64> {
        *self.oversized.lock().expect("not poisoned")
    }

    /// Delete an image pool: unmount it, detach it, and remove the image with
    /// everything in it. [`Self::open`] then makes a new one. The way to
    /// shrink an image whose contents do not fit the smaller size.
    ///
    /// Refused for a device or an existing mount: those are the operator's,
    /// and never formatted twice by the worker.
    pub async fn destroy(self) -> Result<()> {
        let Backing::Image { path, .. } = self.backing.clone() else {
            bail!(
                "only an image pool is destroyed and made again; a device or a mount is the operator's"
            );
        };
        self.unmount().await?;
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        tracing::info!("pool image {} removed", path.display());
        Ok(())
    }

    /// Unmount the pool, and detach an image's loop device. An existing mount
    /// is the operator's, and is left mounted.
    pub async fn unmount(self) -> Result<()> {
        if matches!(self.backing, Backing::Mount) {
            return Ok(());
        }
        privileged(&["umount".as_ref(), self.mountpoint.as_os_str()])
            .await
            .with_context(|| format!("unmounting the pool at {}", self.mountpoint.display()))?;
        // The loop device the mount attached autoclears with it.
        Ok(())
    }

    async fn enable_quotas(&self) -> Result<()> {
        match self.qgroups.mode().as_deref() {
            Some("squota") => Ok(()),
            // Full qgroups on an existing mount: its operator turned them on,
            // and switching them to simple quotas throws their accounting
            // away. That is theirs to do.
            Some("qgroup") => bail!(
                "{} has full btrfs quotas enabled; the pool needs simple quotas. Run \
                 `btrfs quota disable {0}` and let the worker enable simple quotas",
                self.mountpoint.display()
            ),
            _ => {
                privileged(&[
                    "btrfs".as_ref(),
                    "quota".as_ref(),
                    "enable".as_ref(),
                    "--simple".as_ref(),
                    self.mountpoint.as_os_str(),
                ])
                .await
                .with_context(|| {
                    format!(
                        "enabling simple quotas on {} (kernel 6.7 or later)",
                        self.mountpoint.display()
                    )
                })?;
                Ok(())
            }
        }
    }

    /// Set the total everything in the pool may use, and size an image to
    /// follow it.
    ///
    /// The limit applies at once: it is the total's qgroup, and a write past
    /// it fails however much was written before. An image grows online. It
    /// also shrinks online when what the pool holds fits in the smaller size
    /// -- btrfs moves data out of the part being cut off first. When it does
    /// not fit, the image keeps its size and says so ([`Resize::TooFull`]);
    /// the total still binds.
    pub async fn set_total(&self, total: u64) -> Result<Resize> {
        if !self.qgroups.exists(TOTAL) {
            self.btrfs(&["qgroup", "create", &TOTAL.to_string()])
                .await?;
        }
        // Before any resize: nothing new may land in space about to go.
        self.btrfs(&["qgroup", "limit", &total.to_string(), &TOTAL.to_string()])
            .await?;
        *self.oversized.lock().expect("not poisoned") = None;
        let (Backing::Image { path, reserve }, Some(device)) = (&self.backing, &self.loop_device)
        else {
            return self.resize_filesystem(total).await;
        };
        let want = size_for(total);
        let have = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if want > have {
            size_image(path, want, *reserve)?;
            privileged(&["losetup".as_ref(), "-c".as_ref(), device.as_os_str()]).await?;
            self.btrfs(&["filesystem", "resize", "max"]).await?;
            tracing::info!(
                "pool image grown to {} for a total of {}",
                gib(want),
                gib(total)
            );
        } else if want < have {
            // The filesystem first, then the file under it. The other order
            // would cut off whatever btrfs had stored at the end.
            match self
                .btrfs(&["filesystem", "resize", &want.to_string()])
                .await
            {
                Ok(()) => {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(path)
                        .and_then(|f| f.set_len(want))
                        .with_context(|| format!("truncating {}", path.display()))?;
                    privileged(&["losetup".as_ref(), "-c".as_ref(), device.as_os_str()]).await?;
                    tracing::info!(
                        "pool image shrunk to {} for a total of {}",
                        gib(want),
                        gib(total)
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "the pool image stays at {}: what it holds does not fit in {} ({e:#}). \
                         The total of {} applies regardless",
                        gib(have),
                        gib(want),
                        gib(total)
                    );
                    *self.oversized.lock().expect("not poisoned") = Some(want);
                    return Ok(Resize::TooFull {
                        size: have,
                        wanted: want,
                    });
                }
            }
        }
        Ok(Resize::Fits)
    }

    /// Fit a device's or a mount's filesystem to `total` plus its margin, as
    /// far as the device allows: shrink it when the total came down, grow it
    /// back when it went up. The device itself is the operator's -- a zvol's
    /// `volsize`, a partition -- and is never touched; what it frees by
    /// shrinking the filesystem is theirs to reclaim, and the minimum size for
    /// that is logged.
    async fn resize_filesystem(&self, total: u64) -> Result<Resize> {
        let Some(device) = self.single_device().await? else {
            tracing::info!(
                "{} spans several devices; its filesystem is left at its size",
                self.mountpoint.display()
            );
            return Ok(Resize::Fits);
        };
        let capacity = device_capacity(&device.path).await?;
        let wanted = size_for(total);
        if wanted > capacity {
            tracing::error!(
                "WORKER_DISK_MAX asks for {} with its margin, but {} holds {}: the filesystem is \
                 what binds, not the total",
                gib(wanted),
                device.path.display(),
                gib(capacity)
            );
        }
        let target = wanted.min(capacity);
        if target == device.size {
            return Ok(Resize::Fits);
        }
        match self
            .btrfs(&["filesystem", "resize", &target.to_string()])
            .await
        {
            Ok(()) => {
                tracing::info!(
                    "pool filesystem on {} resized from {} to {}",
                    device.path.display(),
                    gib(device.size),
                    gib(target)
                );
                if target < capacity {
                    tracing::info!(
                        "{} could now be shrunk to {} bytes -- never less, which would cut into \
                         the filesystem",
                        device.path.display(),
                        target
                    );
                }
                Ok(Resize::Fits)
            }
            Err(e) => {
                tracing::error!(
                    "the pool filesystem on {} stays at {}: what it holds does not fit in {} \
                     ({e:#}). The total of {} applies regardless",
                    device.path.display(),
                    gib(device.size),
                    gib(target),
                    gib(total)
                );
                Ok(Resize::Kept {
                    size: device.size,
                    wanted: target,
                })
            }
        }
    }

    /// The filesystem's one device and the size the filesystem uses of it;
    /// `None` when it has several.
    async fn single_device(&self) -> Result<Option<FsDevice>> {
        let show = privileged(&[
            "btrfs".as_ref(),
            "filesystem".as_ref(),
            "show".as_ref(),
            "--raw".as_ref(),
            self.mountpoint.as_os_str(),
        ])
        .await?;
        let mut devices = parse_devices(&show);
        Ok(if devices.len() == 1 {
            devices.pop()
        } else {
            None
        })
    }

    /// Bytes free on the filesystem holding a sparse image: what the pool can
    /// still grow into on the host. `None` when the pool's space is its own --
    /// a device, an existing mount, or a reserved image.
    #[must_use]
    pub fn host_free(&self) -> Option<u64> {
        let Backing::Image {
            path,
            reserve: false,
        } = &self.backing
        else {
            return None;
        };
        let dir = path.parent()?;
        let c_path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
        // SAFETY: `c_path` is NUL-terminated and outlives the call; `stat` is a
        // valid out-parameter.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &raw mut stat) };
        // `as`: the field widths differ between targets (32-bit armv7 among
        // them), so no single `From` fits all.
        (rc == 0).then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
    }

    /// A long-lived subvolume at the pool's top, `name`, counted against the
    /// pool's total: the caches, which outlive any one build. Made the first
    /// time with `owner` and `mode`, and left as it is after that.
    ///
    /// `name` is one path component, never a path: what the pool holds and
    /// where is the pool's to decide.
    pub async fn ensure_subvolume(&self, name: &str, owner: Owner, mode: u32) -> Result<PathBuf> {
        if name.is_empty()
            || entry_of(name).is_some()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            bail!("{name:?} is not a name the pool gives a subvolume");
        }
        let path = self.mountpoint.join(name);
        if path.exists() {
            return Ok(path);
        }
        self.btrfs_paths(&["subvolume", "create"], &[&path]).await?;
        let made = async {
            privileged(&[
                "chown".as_ref(),
                owner.chown_spec().as_ref(),
                path.as_os_str(),
            ])
            .await?;
            privileged(&[
                "chmod".as_ref(),
                format!("{mode:o}").as_ref(),
                path.as_os_str(),
            ])
            .await?;
            self.charge_to_total(&path).await
        };
        if let Err(e) = made.await {
            // Never leave one behind that is not counted: the next start would
            // find it and take it as it is.
            let _ = self.btrfs_paths(&["subvolume", "delete"], &[&path]).await;
            return Err(e).with_context(|| format!("preparing {}", path.display()));
        }
        Ok(path)
    }

    /// The entries of the cache subvolume `name` (see
    /// [`Self::ensure_subvolume`]), for the cache code to make, measure and
    /// remove as subvolumes of their own.
    #[must_use]
    pub fn cache_volumes(&self, name: &str) -> CacheVolumes {
        CacheVolumes::new(self.mountpoint.join(name), self.qgroups.clone())
    }

    /// Count the subvolume at `path` against the pool's total. What
    /// `mkarchroot` creates, and anything else made outside [`Self::lease`],
    /// is charged to its own level-0 group and nothing above it until this.
    pub async fn charge_to_total(&self, path: &Path) -> Result<()> {
        let id = subvolume_id(path).await?;
        self.assign(QgroupId::subvolume(id), TOTAL).await
    }

    /// Delete the base chroot, whatever state it is in: what a failed
    /// `mkarchroot` leaves is neither usable nor counted against the total,
    /// and `mkarchroot` refuses to start over on top of it.
    pub async fn discard_root(&self) -> Result<()> {
        self.delete_tree(&self.root()).await
    }

    /// Finish or clear whatever a refresh left behind. Runs at the start of
    /// every refresh and in the startup sweep, while holding the refresh lock
    /// -- which is what lets it treat anything it finds as dead. The two
    /// subvolumes' UUIDs say which side of the exchange it died on: a snapshot
    /// is parented to its origin, so after an exchange `root`'s parent is
    /// `root.next`'s UUID.
    pub async fn recover_refresh(&self) -> Result<()> {
        let root = self.root();
        let next = self.next_root();
        if !next.exists() {
            return Ok(());
        }
        if !root.exists() {
            // No base it could have come from: whatever this is, it is not
            // one. The caller makes the base from nothing, as on a first start.
            self.delete_tree(&next).await?;
            return Ok(());
        }
        // Unreadable UUIDs are an answer too, not an error: which side of the
        // exchange this is cannot be told, and the base itself is whole
        // either way.
        let read = |path: PathBuf| async move {
            subvolume_uuids(&path)
                .await
                .inspect_err(|e| tracing::warn!("{e:#}"))
                .ok()
        };
        match classify_next(read(root.clone()).await, read(next.clone()).await) {
            NextBase::Unfinished => {
                tracing::info!("removing an unfinished base at {}", next.display());
                self.delete_tree(&next).await?;
            }
            NextBase::Previous => {
                // The rename below is what the retire step would have done.
                let prev = self.prev_root();
                if prev.exists() {
                    self.delete_tree(&prev).await?;
                }
                std::fs::rename(&next, &prev)
                    .with_context(|| format!("retiring the previous base at {}", prev.display()))?;
            }
            NextBase::Unknown => {
                // Lose the rollback point rather than guess which one this is.
                tracing::warn!(
                    "removing {} whose relation to the base is unreadable",
                    next.display()
                );
                self.delete_tree(&next).await?;
            }
        }
        Ok(())
    }

    /// Snapshot the base into the refresh's candidate, counted against the
    /// total at once: made at the pool's top, which the total does not cover
    /// on its own. Never left behind uncounted.
    pub async fn snapshot_next(&self) -> Result<PathBuf> {
        let next = self.next_root();
        self.btrfs_paths(&["subvolume", "snapshot"], &[&self.root(), &next])
            .await?;
        if let Err(e) = self.charge_to_total(&next).await {
            let _ = self.delete_tree(&next).await;
            return Err(e).context("counting the refresh candidate against the pool's total");
        }
        Ok(next)
    }

    /// Delete the refresh candidate, whatever state it is in: what a failed
    /// upgrade or check leaves is neither usable nor the base.
    pub async fn discard_next(&self) {
        let next = self.next_root();
        if next.exists()
            && let Err(e) = self.delete_tree(&next).await
        {
            tracing::warn!(
                "could not remove the unfinished base at {} ({e:#})",
                next.display()
            );
        }
    }

    /// Swap the checked candidate into the base's place: `root` names the old
    /// base, then the new one, and is never absent. A build snapshots whatever
    /// it is at the instant it asks, before or after, and one already running
    /// on the old base is unaffected.
    ///
    /// In-process and unprivileged: an exchange within one directory needs no
    /// rights over the subvolumes themselves, and the pool's top belongs to
    /// the worker.
    pub fn exchange_root(&self) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let root = self.root();
        let next = self.next_root();
        let root_c = std::ffi::CString::new(root.as_os_str().as_bytes())?;
        let next_c = std::ffi::CString::new(next.as_os_str().as_bytes())?;
        // SAFETY: two valid NUL-terminated paths; `renameat2` has no other
        // preconditions.
        let rc = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                root_c.as_ptr(),
                libc::AT_FDCWD,
                next_c.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
                .with_context(|| format!("exchanging {} and {}", root.display(), next.display()))
        }
    }

    /// Retire the exchanged-out base as the rollback point. Whatever
    /// `root.prev` was goes first, because a rename onto a non-empty directory
    /// fails. A crash between the exchange and here is finished by
    /// [`Self::recover_refresh`], so this only ever runs to completion or not
    /// at all.
    pub async fn retire_prev(&self) -> Result<()> {
        let prev = self.prev_root();
        if prev.exists() {
            self.delete_tree(&prev).await?;
        }
        std::fs::rename(self.next_root(), &prev)
            .with_context(|| format!("retiring the previous base at {}", prev.display()))?;
        Ok(())
    }

    /// Delete a subvolume with whatever is below it -- or a plain directory,
    /// when that is what is there instead.
    async fn delete_tree(&self, path: &Path) -> Result<()> {
        if path.symlink_metadata().is_err() {
            return Ok(());
        }
        if self
            .btrfs_paths(&["subvolume", "delete", "--recursive"], &[path])
            .await
            .is_err()
        {
            // Not a subvolume after all: a plain directory goes as one.
            privileged(&["rm".as_ref(), "-rf".as_ref(), path.as_os_str()])
                .await
                .with_context(|| format!("removing {}", path.display()))?;
        }
        Ok(())
    }

    /// Make the subvolumes one build runs in: a snapshot of the base chroot,
    /// and an empty one for its workdir, both in one group limited to `limit`
    /// under the pool's total.
    ///
    /// The snapshot is charged nothing for what it shares with the base; only
    /// what the build writes counts.
    pub async fn lease(&self, build_id: i32, limit: Option<u64>) -> Result<BuildVolumes> {
        let root = self.root();
        if !root.join("usr").exists() {
            bail!("the pool has no base chroot yet at {}", root.display());
        }
        let mut volumes = BuildVolumes {
            chroot: self.mountpoint.join(chroot_name(build_id)),
            data: self.mountpoint.join(data_name(build_id)),
            group: QgroupId::build(build_id),
            ids: None,
            released: false,
        };
        // Leftovers under this id, from a crash between making these and
        // recording the build as running.
        self.remove_build(build_id).await;
        // And the groups earlier builds could not take with them.
        self.tidy_groups().await;

        let made = async {
            self.btrfs_paths(&["subvolume", "snapshot"], &[&root, &volumes.chroot])
                .await?;
            self.btrfs_paths(&["subvolume", "create"], &[&volumes.data])
                .await?;
            privileged(&[
                "chown".as_ref(),
                self.owner.chown_spec().as_ref(),
                volumes.data.as_os_str(),
            ])
            .await?;
            if !self.qgroups.exists(volumes.group) {
                self.btrfs(&["qgroup", "create", &volumes.group.to_string()])
                    .await?;
            }
            let ids = SubvolumeIds {
                chroot: subvolume_id(&volumes.chroot).await?,
                data: subvolume_id(&volumes.data).await?,
            };
            for id in [ids.chroot, ids.data] {
                self.assign(QgroupId::subvolume(id), volumes.group).await?;
            }
            self.assign(volumes.group, TOTAL).await?;
            let limit = limit.map_or_else(|| "none".to_string(), |l| l.to_string());
            self.btrfs(&["qgroup", "limit", &limit, &volumes.group.to_string()])
                .await?;
            anyhow::Ok(ids)
        };
        match made.await {
            Ok(ids) => volumes.ids = Some(ids),
            Err(e) => {
                self.remove_build(build_id).await;
                return Err(e).with_context(|| format!("preparing build {build_id}'s volumes"));
            }
        }
        Ok(volumes)
    }

    /// What a build's chroot and working space hold now. Commits first: the
    /// quota figures only move when a transaction does, and a build that just
    /// ended has usually written more than the last commit saw.
    pub async fn build_usage(&self, volumes: &BuildVolumes) -> BuildUsage {
        let Some(ids) = volumes.ids else {
            return BuildUsage::default();
        };
        let _ = self.btrfs(&["filesystem", "sync"]).await;
        let used = |id| {
            self.qgroups
                .usage(QgroupId::subvolume(id))
                .map(|usage| usage.used)
        };
        BuildUsage {
            chroot: used(ids.chroot),
            data: used(ids.data),
        }
    }

    /// Give a build's volumes back. See [`BuildVolumes::release`].
    pub async fn release(&self, mut volumes: BuildVolumes) {
        volumes.released = true;
        let build_id = volumes
            .group
            .build_id()
            .expect("a build's volumes carry a build's group");
        self.remove_build(build_id).await;
    }

    /// Keep a failed build's volumes for `keep_for` instead of deleting them:
    /// renamed to `kept-<id>*`, readable only by the worker, expiring by
    /// mtime. An operator enters the state the build failed in with
    /// `systemd-nspawn -D <pool>/kept-<id>`.
    ///
    /// `tree` is the build's persistent tree, when it has one. It is moved to
    /// `kept-<id>.build` here, after the chroot, so nothing ever sees a kept
    /// tree without its keep, and it expires with the rest. The move is one
    /// rename: a subvolume keeps its id, and so its quota group. A tree that
    /// cannot move -- a plain directory, which `EXDEV`s out of the cache
    /// subvolume -- stays where it is, and [`KeptBuild::tree`] says so; the
    /// caller discards whatever is left. Refused outright for a path outside
    /// the pool: what the pool deletes is the pool's to decide.
    ///
    /// A rename keeps the subvolume ids, so the group keeps its members and
    /// its limit: a kept build can never grow, and it stays under the total.
    /// The name is what tells a kept failure from a crash leftover, which the
    /// sweep must be able to do without any record.
    ///
    /// Never called for an id another kept build already has -- build ids are
    /// unique, and the one-shot id keeps nothing -- so the rename never
    /// collides. On any failure nothing is kept and nothing is leaked: every
    /// name is removed, a tree already moved included.
    pub async fn keep_build(
        &self,
        mut volumes: BuildVolumes,
        keep_for: Duration,
        tree: Option<&Path>,
    ) -> Result<KeptBuild> {
        let build_id = volumes
            .group()
            .build_id()
            .expect("a build's volumes carry a build's group");
        let kept = self.mountpoint.join(kept_name(build_id));
        let kept_data = self.mountpoint.join(kept_data_name(build_id));
        let kept_tree = self.mountpoint.join(kept_build_name(build_id));
        if let Some(tree) = tree
            && !tree.starts_with(&self.mountpoint)
        {
            self.remove_build(build_id).await;
            volumes.released = true;
            bail!("{} is not in the pool; refusing to keep it", tree.display());
        }
        let done = async {
            // As root: the chroot is root-owned, and neither its mtime nor its
            // mode is the renamer's to change. Touched *before* the rename,
            // which leaves a directory's own mtime alone: a `kept-<id>` is
            // never visible with the build's old mtime, which the expiry at
            // every other lease would take for an expired keep.
            privileged(&["touch".as_ref(), volumes.chroot().as_os_str()]).await?;
            std::fs::rename(volumes.chroot(), &kept)
                .with_context(|| format!("keeping the failed build at {}", kept.display()))?;
            std::fs::rename(volumes.data(), &kept_data)
                .with_context(|| format!("keeping the failed build at {}", kept_data.display()))?;
            // As root, since a build that became root in its chroot can own
            // its tree's top. `--no-copy`: a rename or nothing -- `mv` would
            // otherwise copy a plain-directory tree across the subvolume
            // boundary, file by file, charged to nobody's keep.
            let moved = match tree {
                Some(tree) if tree.exists() => match privileged(&[
                    "mv".as_ref(),
                    "--no-copy".as_ref(),
                    "-T".as_ref(),
                    tree.as_os_str(),
                    kept_tree.as_os_str(),
                ])
                .await
                {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::warn!(
                            "the build tree {} stays out of the keep ({e:#})",
                            tree.display()
                        );
                        false
                    }
                },
                _ => false,
            };
            // `0700`, because a running build's chroot is readable by any
            // local user for as long as the build runs, and a kept one would
            // be for days -- and it, its workdir and its tree hold whatever
            // the PKGBUILD fetched with the build credential.
            let mut private: Vec<&OsStr> = vec![
                "chmod".as_ref(),
                "0700".as_ref(),
                kept.as_os_str(),
                kept_data.as_os_str(),
            ];
            if moved {
                private.push(kept_tree.as_os_str());
            }
            privileged(&private).await?;
            // `makechrootpkg` takes a lock beside the copy and leaves it.
            let lock = self
                .mountpoint
                .join(format!("{}.lock", chroot_name(build_id)));
            if lock.exists() {
                let _ = privileged(&["rm".as_ref(), "-f".as_ref(), lock.as_os_str()]).await;
            }
            anyhow::Ok(moved)
        };
        let moved = match done.await {
            Ok(moved) => moved,
            Err(e) => {
                self.remove_build(build_id).await;
                self.remove_kept(build_id).await;
                volumes.released = true;
                return Err(e);
            }
        };
        volumes.released = true;
        // The keep runs from the touch, not from now: an operator's own work
        // at the top of the chroot moves its mtime, and extends it, which is
        // arguably what they want.
        let touched = std::fs::metadata(&kept)
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| SystemTime::now());
        Ok(KeptBuild {
            path: kept,
            until: touched + keep_for,
            tree: moved.then_some(kept_tree),
        })
    }

    /// Remove every build's volumes except those in `keep`, every kept failure
    /// past `keep_failed` (or every one when keeping is off), devtools'
    /// leftover `root.lock`, and the groups they leave behind. Also finishes
    /// an interrupted refresh, when none is running now. Returns how many
    /// builds and kept failures were cleared.
    ///
    /// What makes teardown reliable: `release` loses to `SIGKILL`.
    pub async fn sweep(&self, keep: &HashSet<i32>, keep_failed: Option<Duration>) -> usize {
        let PoolBuilds {
            live: mut builds,
            kept,
        } = self.pool_builds();
        // Kept failures are not crash leftovers: their group stays until they
        // expire, and `tidy_groups` counts them as present for the same reason.
        for id in &kept {
            builds.remove(id);
        }
        builds.retain(|id| !keep.contains(id));
        for &id in &builds {
            self.remove_build(id).await;
        }
        let mut cleared = builds.len();
        // `job-*` parts beside a kept build: a keep interrupted between its
        // two renames. They go without touching the kept build's group.
        for &id in &kept {
            self.remove_build_parts(id).await;
        }
        cleared += self.expire_kept(keep_failed).await;
        // devtools' lock beside the base: unused since the refresh stopped
        // upgrading it in place.
        let root_lock = self.mountpoint.join(ROOT_LOCK);
        if root_lock.exists() {
            let _ = privileged(&["rm".as_ref(), "-f".as_ref(), root_lock.as_os_str()]).await;
        }
        // An interrupted refresh, when none is running now: a refresh in
        // flight recovers for itself at its own start.
        if let Some(_lock) = self.try_refresh_lock()
            && let Err(e) = self.recover_refresh().await
        {
            tracing::warn!("could not recover an interrupted base refresh ({e:#})");
        }
        // A deleted subvolume's own group lingers while any extent it wrote is
        // still referenced, and as an empty `<stale>` group after.
        self.tidy_groups().await;
        let _ = self.btrfs(&["qgroup", "clear-stale"]).await;
        cleared
    }

    /// Delete every kept failure past `keep_failed`, or every one when keeping
    /// is off. Returns how many were removed. Runs in the sweep and before
    /// every lease, so no timer watches over kept builds.
    pub async fn expire_kept(&self, keep_failed: Option<Duration>) -> usize {
        let mut removed = 0;
        for id in self.pool_builds().kept {
            let fresh = keep_failed.is_some_and(|keep| {
                std::fs::metadata(self.mountpoint.join(kept_name(id)))
                    .and_then(|m| m.modified())
                    .is_ok_and(|touched| {
                        touched
                            .checked_add(keep)
                            .is_some_and(|until| until > SystemTime::now())
                    })
            });
            if !fresh {
                self.remove_kept(id).await;
                removed += 1;
            }
        }
        removed
    }

    /// Make room for a build that may use `need` bytes: delete kept failures
    /// oldest first, then the previous base, until `need` fits under the total
    /// or nothing reclaimable is left. Runs before every lease. The caches keep
    /// their own budget-driven reclaim; only what nothing else reclaims -- a
    /// kept failure, a retired base -- is taken here.
    pub async fn reclaim_room(&self, need: u64) {
        if self.fits_under_total(need) {
            return;
        }
        // Slow path only: settle deletions and re-read, so a deletion whose
        // figures have not landed yet does not cost the next kept failure.
        self.settle_quotas().await;
        loop {
            if self.fits_under_total(need) {
                return;
            }
            if self.remove_oldest_kept().await.is_some() {
                self.settle_quotas().await;
                continue;
            }
            if self.prev_root().exists() {
                tracing::info!("reclaiming the previous base chroot for room");
                if self.delete_tree(&self.prev_root()).await.is_ok() {
                    self.settle_quotas().await;
                    continue;
                }
            }
            return;
        }
    }

    /// Whether `need` more bytes fit under the pool's total. True when the
    /// total is unlimited or unreadable: nothing to reclaim against.
    fn fits_under_total(&self, need: u64) -> bool {
        self.total_usage().is_none_or(|usage| {
            usage
                .limit
                .is_none_or(|limit| usage.used.saturating_add(need) <= limit)
        })
    }

    /// Wait out pending deletions and commit, so the quota figures say what
    /// the pool holds now rather than what it held before the last delete.
    async fn settle_quotas(&self) {
        let _ = privileged(&[
            "btrfs".as_ref(),
            "subvolume".as_ref(),
            "sync".as_ref(),
            self.mountpoint.as_os_str(),
        ])
        .await;
        let _ = self.btrfs(&["filesystem", "sync"]).await;
    }

    /// Delete the oldest kept failure, returning its build id; `None` when
    /// there is none.
    async fn remove_oldest_kept(&self) -> Option<i32> {
        // By when it was kept, then by id so the choice is stable. One whose
        // time cannot be read counts as oldest.
        let oldest = self.pool_builds().kept.into_iter().min_by_key(|&id| {
            let touched = std::fs::metadata(self.mountpoint.join(kept_name(id)))
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (touched, id)
        });
        let id = oldest?;
        tracing::info!("reclaiming kept failure {id} for room");
        self.remove_kept(id).await;
        Some(id)
    }

    /// The builds named at the pool's top: live ones (`job-*`, from entries
    /// and from groups, which outlive a crashed worker's entries) and kept
    /// failures (`kept-*`).
    fn pool_builds(&self) -> PoolBuilds {
        let mut builds = self.entry_builds();
        builds.live.extend(
            self.qgroups
                .list()
                .into_iter()
                .filter_map(QgroupId::build_id),
        );
        builds
    }

    /// The builds with entries at the pool's top: what is really there, rather
    /// than what the groups remember.
    fn entry_builds(&self) -> PoolBuilds {
        let mut builds = PoolBuilds::default();
        if let Ok(entries) = std::fs::read_dir(&self.mountpoint) {
            for entry in entries.flatten() {
                match entry_of(&entry.file_name().to_string_lossy()) {
                    Some(Entry::Build { id, .. }) => {
                        builds.live.insert(id);
                    }
                    Some(Entry::Kept { id, .. }) => {
                        builds.kept.insert(id);
                    }
                    _ => {}
                }
            }
        }
        builds
    }

    /// Destroy the groups of builds whose subvolumes are gone.
    ///
    /// A release deletes a build's subvolumes and then its group, but btrfs
    /// refuses to destroy the group while a deleted subvolume's own group
    /// still hangs off it -- which it does until the cleaner has freed that
    /// subvolume, in the background, well after the release returned. Once
    /// the cleaner is done the subvolume's group is `<stale>`, clearing those
    /// empties the build's group, and it can go. Run before each lease, so
    /// they are cleared as builds come and go rather than only at startup.
    async fn tidy_groups(&self) {
        // Entries, not groups: a group outlives its entries, and counting it
        // as its own presence would keep every orphan forever.
        let PoolBuilds { live: builds, kept } = self.entry_builds();
        let orphans: Vec<QgroupId> = self
            .qgroups
            .list()
            .into_iter()
            .filter(|group| {
                group.build_id().is_some_and(|id| {
                    // A kept failure's group has live members: destroying it
                    // would be refused, at every lease, forever.
                    !builds.contains(&id) && !kept.contains(&id)
                })
            })
            .collect();
        if orphans.is_empty() {
            return;
        }
        let _ = self.btrfs(&["qgroup", "clear-stale"]).await;
        for group in orphans {
            // Still refused while the cleaner has not reached it; the next
            // lease tries again.
            if let Err(e) = self.btrfs(&["qgroup", "destroy", &group.to_string()]).await {
                tracing::debug!("qgroup {group} not destroyed yet: {e:#}");
            }
        }
    }

    /// Delete one build's subvolumes and group, whatever is left of them.
    async fn remove_build(&self, build_id: i32) {
        self.remove_volumes(
            vec![chroot_name(build_id), data_name(build_id)],
            Some(format!("{}.lock", chroot_name(build_id))),
            Some(QgroupId::build(build_id)),
        )
        .await;
    }

    /// Delete one kept failure's subvolumes and group, whatever is left of
    /// them: the chroot, the workdir, and the moved build tree, when the
    /// failed build kept one. A kept failure has no lock: the keep removed it.
    async fn remove_kept(&self, build_id: i32) {
        self.remove_volumes(
            vec![
                kept_name(build_id),
                kept_data_name(build_id),
                kept_build_name(build_id),
            ],
            None,
            Some(QgroupId::build(build_id)),
        )
        .await;
    }

    /// Delete one build's `job-*` parts, leaving its group alone: what a keep
    /// interrupted between its two renames leaves beside the kept build.
    async fn remove_build_parts(&self, build_id: i32) {
        self.remove_volumes(
            vec![chroot_name(build_id), data_name(build_id)],
            Some(format!("{}.lock", chroot_name(build_id))),
            None,
        )
        .await;
    }

    /// Delete subvolumes, an optional lock file, and an optional group.
    async fn remove_volumes(
        &self,
        subvolumes: Vec<String>,
        lock: Option<String>,
        group: Option<QgroupId>,
    ) {
        let subvolumes: Vec<PathBuf> = subvolumes
            .into_iter()
            .map(|name| self.mountpoint.join(name))
            .filter(|path| path.exists())
            .collect();
        if !subvolumes.is_empty() {
            let mut args: Vec<&OsStr> = vec![
                "btrfs".as_ref(),
                "subvolume".as_ref(),
                "delete".as_ref(),
                // Anything the build made below its root goes too.
                "--recursive".as_ref(),
            ];
            args.extend(subvolumes.iter().map(|p| p.as_os_str()));
            if let Err(e) = privileged(&args).await {
                tracing::warn!(
                    "could not delete {}: {e:#}",
                    subvolumes
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        if let Some(lock) = lock.map(|name| self.mountpoint.join(name))
            && lock.exists()
        {
            let _ = privileged(&["rm".as_ref(), "-f".as_ref(), lock.as_os_str()]).await;
        }
        if let Some(group) = group
            && self.qgroups.exists(group)
            // Refused while a member still carries charge -- an extent the
            // build wrote that something else still references. The next
            // sweep tries again.
            && let Err(e) = self.btrfs(&["qgroup", "destroy", &group.to_string()]).await
        {
            tracing::debug!("qgroup {group} not destroyed yet: {e:#}");
        }
    }

    async fn assign(&self, child: QgroupId, parent: QgroupId) -> Result<()> {
        // `--no-rescan`: simple quotas never rescan, and a rescan request is
        // an error on them.
        self.btrfs(&[
            "qgroup",
            "assign",
            "--no-rescan",
            &child.to_string(),
            &parent.to_string(),
        ])
        .await
    }

    /// `btrfs <args> <mountpoint>`, as root.
    async fn btrfs(&self, args: &[&str]) -> Result<()> {
        let mut argv: Vec<&OsStr> = vec!["btrfs".as_ref()];
        argv.extend(args.iter().map(OsStr::new));
        argv.push(self.mountpoint.as_os_str());
        privileged(&argv).await.map(drop)
    }

    /// `btrfs <args> <paths...>`, as root.
    async fn btrfs_paths(&self, args: &[&str], paths: &[&Path]) -> Result<()> {
        let mut argv: Vec<&OsStr> = vec!["btrfs".as_ref()];
        argv.extend(args.iter().map(OsStr::new));
        argv.extend(paths.iter().map(|p| p.as_os_str()));
        privileged(&argv).await.map(drop)
    }
}

/// The two subvolumes one build runs in, and the group that limits them.
///
/// Given back with [`Pool::release`]; one that is dropped instead is left for
/// [`Pool::sweep`].
#[derive(Debug)]
pub struct BuildVolumes {
    chroot: PathBuf,
    data: PathBuf,
    group: QgroupId,
    /// The two subvolumes' ids, whose own groups say what each holds. Set
    /// once the lease has made them.
    ids: Option<SubvolumeIds>,
    released: bool,
}

/// The subvolume ids of one build's chroot and working space.
#[derive(Clone, Copy, Debug)]
struct SubvolumeIds {
    chroot: u64,
    data: u64,
}

/// What one build's two subvolumes hold, in bytes as stored. `None` for a
/// part that could not be read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildUsage {
    /// What the build wrote into its chroot snapshot; the base it shares is
    /// charged to the base.
    pub chroot: Option<u64>,
    /// Its working space: source, temporary files, packages.
    pub data: Option<u64>,
}

/// The builds named at the pool's top, by what they are.
#[derive(Debug, Default)]
struct PoolBuilds {
    /// Builds running or left by a crash: `job-*`.
    live: HashSet<i32>,
    /// Kept failures: `kept-*`.
    kept: HashSet<i32>,
}

/// A failed build's volumes, kept for inspection instead of deleted.
#[derive(Clone, Debug)]
pub struct KeptBuild {
    /// The kept chroot, `<pool>/kept-<id>`; the workdir is beside it at
    /// `<pool>/kept-<id>.data`.
    pub path: PathBuf,
    /// When the keep runs out, after which a sweep removes it -- or sooner,
    /// if the pool needs the room.
    pub until: SystemTime,
    /// The build's persistent tree, moved beside the keep as
    /// `<pool>/kept-<id>.build`; `None` when it had none, or it could not be
    /// moved and stayed where it was.
    pub tree: Option<PathBuf>,
}

impl BuildVolumes {
    /// The build's chroot: a snapshot of the base. Its name is the
    /// `makechrootpkg -l` label.
    #[must_use]
    pub fn chroot(&self) -> &Path {
        &self.chroot
    }

    /// The `-l` label `makechrootpkg` knows the chroot by.
    #[must_use]
    pub fn label(&self) -> &str {
        self.chroot
            .file_name()
            .and_then(OsStr::to_str)
            .expect("named by chroot_name")
    }

    /// The build's own writable space: its workdir, owned by the worker.
    #[must_use]
    pub fn data(&self) -> &Path {
        &self.data
    }

    /// The group both are limited by.
    #[must_use]
    pub const fn group(&self) -> QgroupId {
        self.group
    }
}

impl Drop for BuildVolumes {
    fn drop(&mut self) {
        if !self.released {
            tracing::warn!(
                "{} dropped without release; the next sweep removes it",
                self.chroot.display()
            );
        }
    }
}

fn chroot_name(build_id: i32) -> String {
    format!("job-{build_id}")
}

fn data_name(build_id: i32) -> String {
    format!("job-{build_id}.data")
}

fn kept_name(build_id: i32) -> String {
    format!("{KEPT_PREFIX}{build_id}")
}

fn kept_data_name(build_id: i32) -> String {
    format!("{KEPT_PREFIX}{build_id}.data")
}

fn kept_build_name(build_id: i32) -> String {
    format!("{KEPT_PREFIX}{build_id}.build")
}

/// One of the pool's top-level names: every build's volumes, every kept
/// failure, the base in its three generations, and the two lock files. What
/// the sweep, the group tidy and the subvolume guard all read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    Build { id: i32, part: EntryPart },
    Kept { id: i32, part: EntryPart },
    Base(BaseKind),
    RefreshLock,
    DevtoolsLock,
}

/// Which generation of the base a name is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BaseKind {
    Root,
    Next,
    Prev,
}

/// Which part of a build's volumes a name is: its chroot, its workdir, the
/// lock `makechrootpkg` leaves beside the copy, or a kept build's moved tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryPart {
    Chroot,
    Data,
    Lock,
    Build,
}

fn entry_of(name: &str) -> Option<Entry> {
    match name {
        ROOT => Some(Entry::Base(BaseKind::Root)),
        ROOT_NEXT => Some(Entry::Base(BaseKind::Next)),
        ROOT_PREV => Some(Entry::Base(BaseKind::Prev)),
        REFRESH_LOCK => Some(Entry::RefreshLock),
        ROOT_LOCK => Some(Entry::DevtoolsLock),
        _ => {
            let (kept, rest) = name
                .strip_prefix("job-")
                .map(|rest| (false, rest))
                .or_else(|| name.strip_prefix(KEPT_PREFIX).map(|rest| (true, rest)))?;
            let (digits, part) = rest
                .strip_suffix(".data")
                .map(|digits| (digits, EntryPart::Data))
                .or_else(|| {
                    rest.strip_suffix(".lock")
                        .map(|digits| (digits, EntryPart::Lock))
                })
                .or_else(|| {
                    rest.strip_suffix(".build")
                        .map(|digits| (digits, EntryPart::Build))
                })
                .unwrap_or((rest, EntryPart::Chroot));
            let id: i32 = digits.parse().ok().filter(|&id| id > 0)?;
            Some(if kept {
                Entry::Kept { id, part }
            } else {
                Entry::Build { id, part }
            })
        }
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30))
}

/// Whether something is mounted exactly at `path`.
fn is_mountpoint(path: &Path) -> bool {
    std::fs::read_to_string("/proc/self/mountinfo")
        .is_ok_and(|info| mount_points(&info).any(|point| point == path))
}

/// The mount points `/proc/self/mountinfo` lists, unescaped.
fn mount_points(mountinfo: &str) -> impl Iterator<Item = PathBuf> + '_ {
    mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|field| PathBuf::from(unescape_mountinfo(field)))
}

/// mountinfo escapes space, tab, newline and backslash as `\ooo`.
fn unescape_mountinfo(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let code: String = chars.by_ref().take(3).collect();
            match u8::from_str_radix(&code, 8) {
                Ok(byte) => out.push(char::from(byte)),
                Err(_) => {
                    out.push('\\');
                    out.push_str(&code);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// An existing mount is only used if it is a whole btrfs filesystem mounted
/// right there: quotas apply to all of it.
async fn check_dedicated_mount(path: &Path) -> Result<()> {
    let out = query(&[
        "findmnt".as_ref(),
        "--mountpoint".as_ref(),
        path.as_os_str(),
        "--noheadings".as_ref(),
        "--output".as_ref(),
        "FSTYPE,FSROOT".as_ref(),
    ])
    .await
    .with_context(|| format!("{} is not a mount point", path.display()))?;
    let mut fields = out.split_whitespace();
    match (fields.next(), fields.next()) {
        (Some("btrfs"), Some("/")) => Ok(()),
        (Some("btrfs"), Some(root)) => bail!(
            "{} mounts the subvolume {root}, not a whole btrfs filesystem; quotas apply to \
             the whole filesystem, so the pool needs one to itself",
            path.display()
        ),
        (fstype, _) => bail!(
            "{} is {}, not btrfs",
            path.display(),
            fstype.unwrap_or("not mounted")
        ),
    }
}

async fn check_device(device: &Path) -> Result<()> {
    let meta = std::fs::metadata(device)
        .with_context(|| format!("the pool device {}", device.display()))?;
    if !meta.file_type().is_block_device() {
        bail!("{} is not a block device", device.display());
    }
    Ok(())
}

/// The filesystem on a device, `None` if it has none.
async fn filesystem_type(device: &Path) -> Result<Option<String>> {
    // `blkid` exits 2 when it finds nothing to report, which is the answer
    // "empty" rather than a failure.
    let out = crate::cmd::privileged(&[
        "sh".as_ref(),
        "-c".as_ref(),
        r#"blkid -o value -s TYPE "$1" || [ $? -eq 2 ]"#.as_ref(),
        "blkid".as_ref(),
        device.as_os_str(),
    ])
    .await?;
    let fstype = out.trim();
    Ok((!fstype.is_empty()).then(|| fstype.to_string()))
}

/// A device or image that already has a filesystem is used only if it is a
/// pool this worker made.
async fn check_ours(device: &Path) -> Result<()> {
    let fstype = filesystem_type(device).await?;
    let label = privileged(&[
        "blkid".as_ref(),
        "-o".as_ref(),
        "value".as_ref(),
        "-s".as_ref(),
        "LABEL".as_ref(),
        device.as_os_str(),
    ])
    .await
    .unwrap_or_default();
    match (fstype.as_deref(), label.trim()) {
        (Some("btrfs"), LABEL) => Ok(()),
        (fstype, label) => bail!(
            "{} already holds a {} filesystem labelled {label:?}, not an AURCache pool; \
             refusing to use or format it",
            device.display(),
            fstype.unwrap_or("unknown")
        ),
    }
}

async fn mkfs(device: &Path) -> Result<()> {
    privileged(&[
        "mkfs.btrfs".as_ref(),
        "--quiet".as_ref(),
        "--label".as_ref(),
        LABEL.as_ref(),
        "--metadata".as_ref(),
        "single".as_ref(),
        "--data".as_ref(),
        "single".as_ref(),
        device.as_os_str(),
    ])
    .await
    .with_context(|| format!("formatting {}", device.display()))?;
    tracing::info!("created a pool filesystem on {}", device.display());
    Ok(())
}

/// Mount `source` at `mountpoint`. With `via_loop`, `source` is an image
/// file, attached to a loop device by the mount itself.
///
/// Letting `mount` attach it, rather than `losetup` beforehand, is what sets
/// the loop device's autoclear flag: it detaches when the last mount of it
/// goes -- including when a container holding the mount is torn down, which
/// otherwise leaves the device attached to a deleted image for good, its
/// space held and its number taken.
async fn mount(
    source: &Path,
    mountpoint: &Path,
    discard: bool,
    via_loop: bool,
    owner: Owner,
) -> Result<()> {
    std::fs::create_dir_all(mountpoint)
        .with_context(|| format!("creating {}", mountpoint.display()))?;
    let options = mount_options(discard, via_loop);
    privileged(&[
        "mount".as_ref(),
        "-o".as_ref(),
        options.as_ref(),
        source.as_os_str(),
        mountpoint.as_os_str(),
    ])
    .await
    .with_context(|| format!("mounting the pool at {}", mountpoint.display()))?;
    // The pool's top directory belongs to the worker, so its own directories
    // need no root; subvolumes are still made with it.
    privileged(&[
        "chown".as_ref(),
        owner.chown_spec().as_ref(),
        mountpoint.as_os_str(),
    ])
    .await?;
    Ok(())
}

/// The options a pool is mounted with.
fn mount_options(discard: bool, via_loop: bool) -> String {
    let mut options = MOUNT_OPTIONS.to_string();
    // `discard=async` hands space the pool frees back to what holds it. Not
    // for a reserved image, whose point is keeping that space.
    if discard {
        options.push_str(",discard=async");
    }
    if via_loop {
        options.push_str(",loop");
    }
    options
}

/// One device of a btrfs filesystem, as `btrfs filesystem show` lists it.
#[derive(Debug, PartialEq, Eq)]
struct FsDevice {
    path: PathBuf,
    /// How much of the device the filesystem uses.
    size: u64,
}

/// The devices of `btrfs filesystem show --raw`: `devid 1 size N used M path P`.
fn parse_devices(show: &str) -> Vec<FsDevice> {
    show.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.first() != Some(&"devid") {
                return None;
            }
            let after = |key: &str| {
                fields
                    .iter()
                    .position(|f| *f == key)
                    .and_then(|i| fields.get(i + 1))
                    .copied()
            };
            Some(FsDevice {
                size: after("size")?.parse().ok()?,
                path: PathBuf::from(after("path")?),
            })
        })
        .collect()
}

/// A block device's capacity in bytes.
async fn device_capacity(device: &Path) -> Result<u64> {
    let out = privileged(&[
        "blockdev".as_ref(),
        "--getsize64".as_ref(),
        device.as_os_str(),
    ])
    .await?;
    out.trim()
        .parse()
        .with_context(|| format!("the size of {}: {out:?}", device.display()))
}

/// The UUID of the filesystem mounted at `mountpoint`, which names its sysfs
/// directory.
async fn fsid(mountpoint: &Path) -> Result<String> {
    let out = query(&[
        "btrfs".as_ref(),
        "filesystem".as_ref(),
        "show".as_ref(),
        mountpoint.as_os_str(),
    ])
    .await?;
    parse_fsid(&out).with_context(|| format!("no filesystem UUID for {}", mountpoint.display()))
}

fn parse_fsid(show: &str) -> Option<String> {
    show.lines()
        .find_map(|line| line.split_once("uuid:"))
        .map(|(_, uuid)| uuid.trim().to_string())
}

/// A subvolume's UUID and its parent's, which say which side of a refresh it
/// is from: a snapshot is parented to its origin, so after an exchange
/// `root`'s parent is `root.next`'s UUID.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SubvolumeUuids {
    uuid: String,
    /// `None` for a subvolume made outright rather than snapshotted.
    parent: Option<String>,
}

/// What a `root.next` found by recovery is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NextBase {
    /// Snapshotted from `root` and never swapped in: the refresh died before
    /// the exchange.
    Unfinished,
    /// The base `root` was snapshotted from: the refresh died between the
    /// exchange and the retire.
    Previous,
    /// Neither relation holds, or a UUID could not be read.
    Unknown,
}

/// Which side of the exchange a leftover `root.next` is on, from the two
/// subvolumes' UUIDs; `None` for one that could not be read.
fn classify_next(root: Option<SubvolumeUuids>, next: Option<SubvolumeUuids>) -> NextBase {
    let (Some(root), Some(next)) = (root, next) else {
        return NextBase::Unknown;
    };
    if next.parent.as_ref() == Some(&root.uuid) {
        NextBase::Unfinished
    } else if root.parent.as_ref() == Some(&next.uuid) {
        NextBase::Previous
    } else {
        NextBase::Unknown
    }
}

async fn subvolume_uuids(path: &Path) -> Result<SubvolumeUuids> {
    let out = privileged(&[
        "btrfs".as_ref(),
        "subvolume".as_ref(),
        "show".as_ref(),
        path.as_os_str(),
    ])
    .await?;
    parse_uuids(&out).with_context(|| format!("UUIDs of {}", path.display()))
}

/// The `UUID` and `Parent UUID` lines of `btrfs subvolume show`. The parent is
/// `-` for a subvolume made outright, and its origin's UUID for a snapshot.
fn parse_uuids(show: &str) -> Option<SubvolumeUuids> {
    let mut uuid = None;
    let mut parent = None;
    for line in show.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("Parent UUID:") {
            let value = value.trim();
            parent = (value != "-").then(|| value.to_string());
        } else if let Some(value) = line.strip_prefix("UUID:") {
            uuid = Some(value.trim().to_string());
        }
    }
    Some(SubvolumeUuids {
        uuid: uuid?,
        parent,
    })
}

/// A subvolume's id; its own qgroup is `0/<id>`.
async fn subvolume_id(path: &Path) -> Result<u64> {
    let out = query(&[
        "btrfs".as_ref(),
        "inspect-internal".as_ref(),
        "rootid".as_ref(),
        path.as_os_str(),
    ])
    .await?;
    out.trim()
        .parse()
        .with_context(|| format!("subvolume id of {}: {out:?}", path.display()))
}

/// What [`prepare_image`] found at the image's path.
enum Image {
    /// Nothing: a new, empty image was made, which needs formatting.
    Created,
    /// An image from an earlier start, to be checked rather than formatted.
    Existing,
}

/// Create the image if it is missing.
async fn prepare_image(path: &Path, size: u64, reserve: bool) -> Result<Image> {
    if path.exists() {
        return Ok(Image::Existing);
    }
    let dir = path.parent().context("the image path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Before the file exists: on a btrfs host `+C` only takes on an empty
    // file, and a new file inherits it from its directory. Elsewhere the
    // attribute does not exist, which is fine.
    let _ = query(&["chattr".as_ref(), "+C".as_ref(), dir.as_os_str()]).await;
    size_image(path, size, reserve)?;
    Ok(Image::Created)
}

/// Make the image `size` bytes: sparse, or allocated in full when reserving.
fn size_image(path: &Path, size: u64, reserve: bool) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    if reserve {
        use std::os::fd::AsRawFd;
        let len = libc::off_t::try_from(size).context("image size")?;
        // SAFETY: a valid descriptor and an in-range length; `fallocate` has
        // no other preconditions.
        let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, len) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("reserving {} for {}", gib(size), path.display()));
        }
    } else {
        file.set_len(size)
            .with_context(|| format!("sizing {}", path.display()))?;
    }
    Ok(())
}

/// The loop device `path` is already attached to, after a worker restart.
async fn attached_loop(path: &Path) -> Option<PathBuf> {
    let out = privileged(&["losetup".as_ref(), "-j".as_ref(), path.as_os_str()])
        .await
        .ok()?;
    out.lines()
        .next()
        .and_then(|line| line.split_once(':'))
        .map(|(device, _)| PathBuf::from(device))
}

/// Make sure the loop device the kernel hands out next has a node.
///
/// Inside a container `/dev` only has the loop nodes that existed when it
/// started, and a mount asking for a new device would find no node for it.
/// `losetup --find` names that device -- with ` (lost)` after it when its node
/// is missing -- so the node is made first.
async fn ensure_free_loop_node() -> Result<()> {
    let out = privileged(&["losetup", "--find"])
        .await
        .context("finding a free loop device")?;
    let device =
        free_loop_device(&out).with_context(|| format!("unexpected `losetup --find`: {out:?}"))?;
    if !device.path.exists() {
        privileged(&[
            "mknod".as_ref(),
            device.path.as_os_str(),
            "b".as_ref(),
            "7".as_ref(),
            device.minor.to_string().as_ref(),
        ])
        .await
        .with_context(|| format!("creating {}", device.path.display()))?;
    }
    Ok(())
}

/// A loop device, by node path and minor number.
#[derive(Debug, PartialEq, Eq)]
struct LoopDevice {
    path: PathBuf,
    minor: u32,
}

/// The device `losetup --find` printed, ignoring the ` (lost)` it adds when
/// the node does not exist.
fn free_loop_device(out: &str) -> Option<LoopDevice> {
    let name = out.split_whitespace().next()?;
    let minor = name.strip_prefix("/dev/loop")?.parse().ok()?;
    Some(LoopDevice {
        path: PathBuf::from(name),
        minor,
    })
}

/// ZFS, from `statfs`.
const ZFS_SUPER_MAGIC: i64 = 0x2FC1_2FC1;

/// Say how to tune the dataset an image sits on, when it is ZFS: a nested
/// btrfs pays for ZFS's defaults on every write. The worker cannot read the
/// dataset's properties from inside a container, so it says what they should
/// be rather than whether they are.
fn advise_on_zfs(image: &Path) {
    let Some(dir) = image.parent() else {
        return;
    };
    if crate::volumes::fs_type(dir) == Some(ZFS_SUPER_MAGIC) {
        tracing::info!(
            "the pool image {} is on ZFS: its dataset should have recordsize=16K (or 32K), \
             primarycache=metadata and logbias=throughput, or the pool a zvol of its own \
             (WORKER_POOL) -- see the worker configuration docs, \"Tuning an image on ZFS\"",
            image.display()
        );
    }
}

/// Switch a loop device to direct I/O, which keeps the image's data out of
/// the host's page cache, where it would sit twice. A host filesystem that
/// refuses it keeps buffered I/O, which works the same, only slower.
async fn enable_direct_io(device: &Path) {
    if let Err(e) = privileged(&[
        "losetup".as_ref(),
        "--direct-io=on".as_ref(),
        device.as_os_str(),
    ])
    .await
    {
        tracing::info!("direct I/O refused for the pool image ({e:#}); using buffered I/O");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inside a container, `losetup --find` marks a device whose node is
    /// missing -- the one case the name has to survive.
    #[test]
    fn a_free_loop_device_is_read_with_or_without_its_node() {
        let device = |path: &str, minor| LoopDevice {
            path: PathBuf::from(path),
            minor,
        };
        assert_eq!(
            free_loop_device("/dev/loop5\n"),
            Some(device("/dev/loop5", 5))
        );
        assert_eq!(
            free_loop_device("/dev/loop5 (lost)\n"),
            Some(device("/dev/loop5", 5))
        );
        assert_eq!(free_loop_device(""), None);
        assert_eq!(free_loop_device("/dev/sda"), None);
    }

    #[test]
    fn a_pool_image_is_mounted_through_a_loop_device_of_its_own() {
        assert!(mount_options(true, true).ends_with(",discard=async,loop"));
        assert!(!mount_options(false, false).contains("discard"));
        assert!(!mount_options(false, false).contains("loop"));
    }

    #[test]
    fn a_filesystems_devices_come_from_filesystem_show() {
        let show = "Label: 'aurcache-chroot'  uuid: 40207968-ec27-482a-8e8d-46e4389be924\n\
                    \tTotal devices 1 FS bytes used 222537543680\n\
                    \tdevid    1 size 1099511627776 used 252354494464 path /dev/sdc\n";
        assert_eq!(
            parse_devices(show),
            [FsDevice {
                path: PathBuf::from("/dev/sdc"),
                size: 1_099_511_627_776
            }]
        );
        let two = format!("{show}\tdevid    2 size 5 used 1 path /dev/sdd\n");
        assert_eq!(parse_devices(&two).len(), 2);
    }

    #[test]
    fn the_margin_is_a_twentieth_but_never_under_two_gib() {
        assert_eq!(size_for(200 << 30), 210 << 30);
        assert_eq!(size_for(10 << 30), (10 << 30) + MIN_MARGIN);
    }

    #[test]
    fn pool_entries_name_what_they_are() {
        use BaseKind::{Next, Prev, Root};
        use EntryPart::{Build, Chroot, Data, Lock};
        assert_eq!(
            entry_of("job-42"),
            Some(Entry::Build {
                id: 42,
                part: Chroot
            })
        );
        assert_eq!(
            entry_of("job-42.data"),
            Some(Entry::Build { id: 42, part: Data })
        );
        assert_eq!(
            entry_of("job-42.lock"),
            Some(Entry::Build { id: 42, part: Lock })
        );
        assert_eq!(
            entry_of("kept-42"),
            Some(Entry::Kept {
                id: 42,
                part: Chroot
            })
        );
        assert_eq!(
            entry_of("kept-42.data"),
            Some(Entry::Kept { id: 42, part: Data })
        );
        assert_eq!(
            entry_of("kept-42.build"),
            Some(Entry::Kept {
                id: 42,
                part: Build
            })
        );
        assert_eq!(
            entry_of("job-42.build"),
            Some(Entry::Build {
                id: 42,
                part: Build
            })
        );
        assert_eq!(entry_of("root"), Some(Entry::Base(Root)));
        assert_eq!(entry_of("root.next"), Some(Entry::Base(Next)));
        assert_eq!(entry_of("root.prev"), Some(Entry::Base(Prev)));
        assert_eq!(entry_of("refresh.lock"), Some(Entry::RefreshLock));
        assert_eq!(entry_of("root.lock"), Some(Entry::DevtoolsLock));
        assert_eq!(entry_of("cache"), None);
        assert_eq!(entry_of("job-x"), None);
        assert_eq!(entry_of("job-0"), None);
        assert_eq!(entry_of("job--1"), None);
        assert_eq!(entry_of("kept-x"), None);
        assert_eq!(entry_of("root.next.prev"), None);
    }

    /// The two UUID lines of `btrfs subvolume show`, as recovery reads them:
    /// a snapshot is parented to its origin, and a made subvolume to `-`.
    #[test]
    fn subvolume_uuids_come_from_subvolume_show() {
        let show = "\
job-7
\tName: \t\t\tjob-7
\tUUID: \t\t\t11111111-2222-3333-4444-555555555555
\tParent UUID: \t\taaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee
\tReceived UUID: \t\t-
\tCreation time: \t\t2026-09-28 12:00:00 +0000
";
        assert_eq!(
            parse_uuids(show),
            Some(SubvolumeUuids {
                uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                parent: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string())
            })
        );
        let made = show.replace(
            "Parent UUID: \t\taaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "Parent UUID: \t\t-",
        );
        assert_eq!(
            parse_uuids(&made),
            Some(SubvolumeUuids {
                uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                parent: None
            })
        );
        assert_eq!(parse_uuids("no uuid here\n"), None);
    }

    /// Which side of the exchange a leftover `root.next` is on -- and an
    /// unreadable UUID is an answer, not an error: it is deleted, never left
    /// to fail every refresh.
    #[test]
    fn a_leftover_candidate_is_classified_by_its_uuids() {
        let volume = |uuid: &str, parent: Option<&str>| {
            Some(SubvolumeUuids {
                uuid: uuid.to_string(),
                parent: parent.map(str::to_string),
            })
        };
        assert_eq!(
            classify_next(volume("a", None), volume("b", Some("a"))),
            NextBase::Unfinished
        );
        assert_eq!(
            classify_next(volume("b", Some("a")), volume("a", None)),
            NextBase::Previous
        );
        assert_eq!(
            classify_next(volume("a", None), volume("c", None)),
            NextBase::Unknown
        );
        assert_eq!(classify_next(volume("a", None), None), NextBase::Unknown);
        assert_eq!(
            classify_next(None, volume("b", Some("a"))),
            NextBase::Unknown
        );
    }

    #[test]
    fn mountinfo_points_are_unescaped() {
        let info = "36 35 98:0 / /mnt/pool rw,noatime shared:1 - btrfs /dev/loop1 rw\n\
                    37 35 98:1 / /mnt/with\\040space rw - ext4 /dev/sda1 rw\n";
        let points: Vec<_> = mount_points(info).collect();
        assert_eq!(
            points,
            [PathBuf::from("/mnt/pool"), PathBuf::from("/mnt/with space")]
        );
    }

    #[test]
    fn the_fsid_comes_from_filesystem_show() {
        let show = "Label: 'aurcache-pool'  uuid: cbfa34ed-0092-4af9-bf6f-bab7c0c3025f\n\
                    \tTotal devices 1 FS bytes used 144.00KiB\n";
        assert_eq!(
            parse_fsid(show).as_deref(),
            Some("cbfa34ed-0092-4af9-bf6f-bab7c0c3025f")
        );
        assert_eq!(parse_fsid("nothing"), None);
    }
}
