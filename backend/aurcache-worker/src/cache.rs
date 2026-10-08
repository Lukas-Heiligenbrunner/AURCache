//! Worker-local build caches (best-effort). Persisted under the cache volume
//! and bind-mounted into each `makechrootpkg` copy:
//!
//! * per-pkgbase `SRCDEST` — incremental git fetch / tarball reuse,
//! * a persistent `GNUPGHOME` — keys stay trusted across updates,
//! * the pacman package cache.
//!
//! Every accessor is graceful: a missing directory is simply created, and any
//! I/O error is logged and never propagated into the build.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime};

/// The part of a `repo.db` that tells us whether a cached archive is current,
/// by filename.
pub type RepoDb = HashMap<String, RepoDbEntry>;

/// What `repo.db` says one package file should be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoDbEntry {
    /// `%CSIZE%`: the archive's size in bytes.
    pub csize: u64,
    /// `%SHA256SUM%`, hex.
    pub sha256: String,
}

/// Handle to the on-disk cache layout.
#[derive(Clone, Debug)]
pub struct Cache {
    root: PathBuf,
    max_size: u64,
    ttl: Duration,
    pkg_max_size: u64,
    pkg_ttl: Duration,
    /// Present when the cache is the storage pool's: then each package's
    /// sources and each kept build tree is a subvolume of its own, measured by
    /// its quota group and removed by deleting it. Entries from before that --
    /// plain directories -- are measured and removed the old way.
    volumes: Option<aurcache_chroot::CacheVolumes>,
}

/// What [`Cache::discard_builddir`] did with a package's build tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Discard {
    /// The tree was there and is gone.
    Removed,
    /// There was no tree: the build failed before making one, or a keep took
    /// it.
    #[default]
    Absent,
    /// The tree was there and could not be removed.
    Failed,
}

/// A per-pkgbase source cache entry considered for eviction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheEntry {
    pub pkgbase: String,
    pub size: Option<u64>,
    pub last_used: SystemTime,
}

impl Cache {
    pub fn new(
        root: &Path,
        max_size: u64,
        ttl_secs: u64,
        pkg_max_size: u64,
        pkg_ttl_secs: u64,
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            max_size,
            ttl: Duration::from_secs(ttl_secs),
            pkg_max_size,
            pkg_ttl: Duration::from_secs(pkg_ttl_secs),
            volumes: None,
        }
    }

    /// Make new entries subvolumes in the pool; see [`Cache::volumes`].
    #[must_use]
    pub fn with_volumes(mut self, volumes: Option<aurcache_chroot::CacheVolumes>) -> Self {
        self.volumes = volumes;
        self
    }

    /// An entry of the cache -- one package's sources, one kept build tree --
    /// made if missing: a subvolume where the cache is the pool's, otherwise
    /// a directory. `None` only if neither could be made.
    fn entry(&self, path: PathBuf) -> Option<PathBuf> {
        let Some(volumes) = &self.volumes else {
            return Self::ensured(path);
        };
        // The same owner, group and mode `ensured` leaves a directory with:
        // the worker's, the parent's group (setgid: the build user's), and
        // group-writable, so the build user writes into it.
        let parent = Self::ensured(path.parent()?.to_path_buf())?;
        let owner = aurcache_chroot::Owner {
            uid: aurcache_chroot::Owner::current().uid,
            gid: {
                use std::os::unix::fs::MetadataExt;
                std::fs::metadata(&parent).ok()?.gid()
            },
        };
        match volumes.ensure(&path, owner, 0o2775) {
            Ok(()) => Some(path),
            Err(e) => {
                // Still a place to build: a directory works, only without the
                // subvolume's cheap size and removal.
                tracing::warn!(
                    "{} not made a subvolume ({e:#}); using a directory",
                    path.display()
                );
                Self::ensured(path)
            }
        }
    }

    /// Bytes an entry holds: its quota group's figure when it is a subvolume,
    /// otherwise walked.
    fn entry_size(&self, path: &Path) -> Option<u64> {
        self.volumes.as_ref().and_then(|v| v.usage(path))
    }

    /// Remove an entry: deleted as a subvolume, at once, when it is one;
    /// otherwise set aside and removed as a tree ([`remove_tree`]).
    fn remove_entry(&self, path: &Path) -> std::io::Result<()> {
        match &self.volumes {
            Some(volumes) if volumes.is_volume(path) => volumes
                .remove(path)
                .map_err(|e| std::io::Error::other(format!("{e:#}"))),
            _ => remove_tree(path),
        }
    }

    /// `SRCDEST` for a pkgbase; created on demand. Returns `None` only if the
    /// directory truly cannot be created (build falls back to an ephemeral dir).
    pub fn srcdest(&self, pkgbase: &str) -> Option<PathBuf> {
        self.entry(self.root.join("srcdest").join(sanitize(pkgbase)))
    }

    /// Bytes a package's source cache holds, as stored, when it is a
    /// subvolume; `None` otherwise -- a plain directory is not walked for a
    /// report. Never creates anything.
    #[must_use]
    pub fn source_size(&self, pkgbase: &str) -> Option<u64> {
        self.entry_size(&self.root.join("srcdest").join(sanitize(pkgbase)))
    }

    /// Bytes a package's kept build tree holds: its quota group's figure when
    /// it is a subvolume, else the size recorded after its last build. `None`
    /// when there is neither. Never walks or creates anything.
    #[must_use]
    pub fn build_tree_size(&self, platform: &str, pkgbase: &str) -> Option<u64> {
        let tree = self.builddir_tree_path(platform, pkgbase);
        self.entry_size(&tree).or_else(|| {
            std::fs::read_to_string(self.size_stamp(platform, pkgbase))
                .ok()?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
    }

    /// Persistent build tree root for one architecture, bound over `/build`
    /// when a package asks to keep its tree between builds.
    ///
    /// Keyed by platform and *not* by package: makepkg already namespaces the
    /// tree as `$BUILDDIR/$pkgbase/src`, so keying by package here would nest
    /// the name twice. Platform must be in the key though -- a build tree holds
    /// compiled objects, unlike `srcdest`, whose downloads are architecture
    /// independent -- and it also keeps a native and an emulated build of one
    /// package from sharing a tree on the same worker.
    ///
    /// Only ever populated for packages that opted in, because the bind only
    /// happens for those; see `design/implemented/persistent-build-directory.md`.
    pub fn builddir(&self, platform: &str) -> Option<PathBuf> {
        Self::ensured(self.root.join("builddir").join(sanitize(platform)))
    }

    /// One package's persistent build tree, `$BUILDDIR/$pkgbase`: what a
    /// build is given at `/build/<pkgbase>`, rather than the whole
    /// [`Self::builddir`] with every other package's tree in it.
    pub fn builddir_tree(&self, platform: &str, pkgbase: &str) -> Option<PathBuf> {
        self.builddir(platform)?;
        self.entry(self.builddir_tree_path(platform, pkgbase))
    }

    /// The file caching a tree's measured size, written after each build so
    /// reclaim never has to walk one.
    ///
    /// Beside the trees rather than inside them. A tree is the build user's --
    /// makepkg creates it, mode 755 -- so the worker cannot write into it, and
    /// a stamp kept there was never once written: every reclaim walked every
    /// tree. `.sizes` is not a platform name, so it is never taken for a
    /// platform's tree root.
    fn size_stamp(&self, platform: &str, pkgbase: &str) -> PathBuf {
        self.root
            .join("builddir")
            .join(".sizes")
            .join(sanitize(platform))
            .join(sanitize(pkgbase))
    }

    /// Record how big a package's persistent tree is, so reclaim can total the
    /// cache without walking it.
    ///
    /// Called once after a build, where the cost rides on top of something that
    /// already took minutes or hours. Measuring during reclaim instead would
    /// mean walking every candidate on every build, and these trees reach
    /// 130 GB and millions of files. Blocking: run it off the async runtime.
    pub fn record_builddir_size(&self, platform: &str, pkgbase: &str) {
        let Some(tree) = self.builddir(platform).map(|r| r.join(sanitize(pkgbase))) else {
            return;
        };
        // A subvolume's size is its quota group's, always current.
        if !tree.is_dir() || self.entry_size(&tree).is_some() {
            return;
        }
        if let Some(size) = measure_tree(&tree) {
            self.write_size_stamp(platform, pkgbase, size);
        } else {
            let _ = std::fs::remove_file(self.size_stamp(platform, pkgbase));
        }
    }

    fn write_size_stamp(&self, platform: &str, pkgbase: &str, size: u64) {
        let stamp = self.size_stamp(platform, pkgbase);
        let written = stamp
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&stamp, size.to_string()));
        if let Err(e) = written {
            tracing::warn!(
                "could not record build tree size in {}: {e}",
                stamp.display()
            );
        }
    }

    /// Drop persistent build trees, least recently used first, until the cache
    /// is within `max_bytes` *and* the filesystem has `min_free` bytes spare.
    /// Never touches the tree of a package in `in_use`: this build's own, and
    /// every sibling's still running. Age cannot tell those apart -- a tree's
    /// mtime only moves when an entry directly under it changes, so an
    /// hours-long compile can leave its tree looking like the oldest here.
    ///
    /// **Nothing is evicted while the cache is within its limits.** Disk that
    /// nothing else needs is not worth reclaiming, and a tree kept is a rebuild
    /// avoided, so eviction is only ever a response to pressure -- never a
    /// tidy-up. It follows that a tree belonging to a deleted package is left
    /// alone until the space is actually wanted, which is the right time to
    /// notice: a worker is never told that a package went away, and being
    /// unused is the only evidence available.
    ///
    /// Least recently used, and deliberately nothing cleverer. Ranking by what
    /// a rebuild costs was tried and removed: the time a tree saves is roughly
    /// proportional to what it holds -- most of it is download the tree spares
    /// you -- so cost per byte comes out near constant and sorts nothing.
    /// Measured, it spans about 5x across packages as different as `libpng12`
    /// (17.9 MB, saves 8 s) and `unreal-engine` (130 GB, saves hours), against
    /// the 500x spread that *build duration* suggested. Duration was the wrong
    /// number: a warm `libpng12` build still took 11 s of the cold 19 s, so the
    /// tree saved 8 s and not 19.
    ///
    /// Two limits, because each is useless alone. A free-space floor does
    /// nothing on a large pool -- trees would grow into the terabytes before it
    /// triggered -- so `max_bytes` is what bounds the cache whatever the
    /// storage underneath it. A cap alone cannot see the rest of the machine,
    /// so the floor still covers a small disk, or one shared with something
    /// else that grew.
    ///
    /// Sizes come from the stamp each build leaves behind; a tree without one
    /// is walked once and stamped. So the usual case totals the cache by
    /// reading a handful of small files.
    ///
    /// Whole trees, never partial contents: half a tree is worse than none,
    /// because makepkg would treat it as resumable.
    ///
    /// Best-effort. Failing to reclaim is worth reporting and carrying on;
    /// refusing to build over it would turn a full disk into an idle worker.
    /// Blocking, and a tree can take minutes to delete: run it off the async
    /// runtime.
    pub fn reclaim_builddirs(
        &self,
        platform: &str,
        in_use: &[String],
        max_bytes: u64,
        min_free: u64,
    ) {
        let Some(root) = self.builddir(platform) else {
            return;
        };
        // Finish any removal an earlier pass started and could not complete.
        sweep_set_aside(&root);

        // Oldest first. `mtime` on the tree root moves whenever a build writes
        // into it, which makes it a serviceable "last used" -- and a tree whose
        // package was deleted simply stops being touched, so it drifts to the
        // front on its own.
        let mut trees: Vec<(std::time::SystemTime, PathBuf, Option<u64>)> =
            std::fs::read_dir(&root)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| !is_set_aside(&e.file_name()))
                .filter_map(|e| {
                    let modified = e.metadata().ok()?.modified().ok()?;
                    let path = e.path();
                    let size = self.stamped_size(platform, &path);
                    Some((modified, path, size))
                })
                .collect();
        trees.sort_by_key(|(modified, _, _)| *modified);

        let mut total = trees
            .iter()
            .filter_map(|(_, _, size)| *size)
            .fold(0_u64, u64::saturating_add);
        let mut unknown = trees.iter().filter(|(_, _, size)| size.is_none()).count();

        // A set: tested per tree below, which is quadratic over the slice.
        let in_use: std::collections::HashSet<&str> = in_use.iter().map(String::as_str).collect();
        // `statvfs` once up front, then only after a removal freed something:
        // free space cannot change while this loop merely looks.
        let mut short_of_free = min_free > 0 && free_bytes(&root).is_some_and(|f| f < min_free);
        for (_, path, size) in trees {
            let over_budget = max_bytes > 0 && (unknown > 0 || total > max_bytes);
            if !over_budget && !short_of_free {
                return;
            }
            // A tree a build is using is worth keeping even when it is itself
            // what breaches the budget: deleting it fails that build.
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| in_use.contains(n))
            {
                continue;
            }
            match self.remove_entry(&path) {
                Ok(()) => {
                    if let Some(size) = size {
                        total = total.saturating_sub(size);
                    } else {
                        unknown -= 1;
                    }
                    short_of_free = min_free > 0 && free_bytes(&root).is_some_and(|f| f < min_free);
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        let _ = std::fs::remove_file(self.size_stamp(platform, name));
                    }
                    tracing::info!(
                        "reclaimed persistent build tree {} ({size:?} bytes)",
                        path.display()
                    );
                }
                Err(e) => tracing::warn!("could not reclaim {}: {e}", path.display()),
            }
        }
    }

    /// A tree's recorded size, measuring and stamping it if no stamp is there.
    /// Tolerates a stamp carrying more than one field, from the brief time a
    /// rebuild cost was recorded alongside.
    fn stamped_size(&self, platform: &str, tree: &Path) -> Option<u64> {
        if let Some(size) = self.entry_size(tree) {
            return Some(size);
        }
        let Some(name) = tree.file_name().and_then(|n| n.to_str()) else {
            return measure_tree(tree);
        };
        if let Ok(text) = std::fs::read_to_string(self.size_stamp(platform, name))
            && let Some(Ok(size)) = text.split_whitespace().next().map(str::parse)
        {
            return Some(size);
        }
        let size = measure_tree(tree);
        if let Some(size) = size {
            self.write_size_stamp(platform, name, size);
        }
        size
    }

    /// Delete a package's persistent build tree outright, with its size stamp:
    /// what an unsuccessful build leaves is half-updated, which makepkg treats
    /// as resumable, and the next build starts cold instead. A subvolume goes
    /// at once; a plain directory is set aside and removed as a tree.
    ///
    /// Failure, cancel, timeout, out-of-memory and out-of-disk all go through
    /// a kill or a half-finished step, so every one of them can leave the tree
    /// half-updated -- a stale checkout, or an output a killed compiler left
    /// truncated but newer than its inputs, which make and ninja then treat as
    /// built. Every retry would fail identically; starting cold is always safe.
    ///
    /// Best-effort, like every other wipe here: the next build rebuilds over
    /// -- or without -- whatever this leaves. Blocking: run it off the async
    /// runtime.
    pub fn discard_builddir(&self, platform: &str, pkgbase: &str) -> Discard {
        // The stamp goes whether or not a tree is left: a keep may have taken
        // the tree already, and the stamp describes a tree that is gone.
        let _ = std::fs::remove_file(self.size_stamp(platform, pkgbase));
        let tree = self.builddir_tree_path(platform, pkgbase);
        if tree.symlink_metadata().is_err() {
            return Discard::Absent;
        }
        match self.remove_entry(&tree) {
            Ok(()) => Discard::Removed,
            Err(e) => {
                tracing::warn!("could not discard build tree {}: {e}", tree.display());
                Discard::Failed
            }
        }
    }

    /// Where a package's persistent tree is, whether or not it exists; never
    /// makes it (see [`Self::builddir_tree`] for that).
    #[must_use]
    pub fn builddir_tree_path(&self, platform: &str, pkgbase: &str) -> PathBuf {
        self.root
            .join("builddir")
            .join(sanitize(platform))
            .join(sanitize(pkgbase))
    }

    /// Shared persistent GnuPG home for validpgpkeys.
    pub fn gnupg_home(&self) -> Option<PathBuf> {
        Self::ensured(self.root.join("gnupg"))
    }

    /// One build's private replica of that keyring, staged by the worker and
    /// written by nothing once the build starts.
    ///
    /// The build cannot read the shared keyring directly. Source signatures are
    /// verified on the worker, as the build user, while sibling jobs may be
    /// importing keys into it — and a keybox rewritten under a reader fails the
    /// verification whether or not gpg takes its dotlock. A replica has no
    /// writer for the life of the build, which no shared keyring can offer.
    ///
    /// Deliberately not group-writable, unlike every other directory here: the
    /// build only reads it, so leaving it read-only keeps a PKGBUILD out of the
    /// trust store its own sources are checked against.
    pub fn gnupg_job(&self, label: &str) -> Option<PathBuf> {
        Self::ensured_readable(self.root.join(format!("gnupg-{label}")))
    }

    /// Discard a job's keyring replica when its build ends.
    pub fn wipe_gnupg_job(&self, label: &str) {
        wipe_dir(&self.root.join(format!("gnupg-{label}")), "job keyring");
    }

    /// Shared pacman package cache, bound **read-only** into every chroot as
    /// the second `CacheDir`, so builds get hits without being able to write.
    pub fn pacman_pkg(&self) -> Option<PathBuf> {
        Self::ensured(self.root.join("pacman-pkg"))
    }

    /// Private writable pacman cache for one job, bound over
    /// `/var/cache/pacman/pkg` so downloads cannot collide.
    ///
    /// Concurrent builds otherwise share one writable cache — `arch-nspawn`
    /// bind-mounts the host's first `CacheDir` read-write into every chroot —
    /// and two jobs downloading the same dependency race on the same
    /// `<pkg>.part` file, leaving a corrupt archive behind.
    pub fn pacman_pkg_job(&self, label: &str) -> Option<PathBuf> {
        Self::ensured(self.root.join(format!("pacman-pkg-{label}")))
    }

    /// Discard a job's private pacman cache once its packages are promoted.
    pub fn wipe_pacman_pkg_job(&self, label: &str) {
        wipe_dir(
            &self.root.join(format!("pacman-pkg-{label}")),
            "job pkg cache",
        );
    }

    /// Move a job's freshly downloaded packages into the shared cache so the
    /// next build gets them as hits.
    ///
    /// `rename` within one filesystem is atomic, so a concurrent build reading
    /// the shared cache sees either the old file or the complete new one, never
    /// a partial write — which is the whole failure this design exists to
    /// avoid. Both directories live under the cache root, so the same-filesystem
    /// requirement holds.
    ///
    /// Runs only after `makechrootpkg` has exited. Promoting mid-build would
    /// move a package out from under the running pacman; a hard link would be
    /// needed instead, and there is no reason to promote early.
    ///
    /// A `repo_db` means the caller has fetched the repository this worker
    /// serves, and every job artifact that the DB names is checked against what
    /// the repository publishes *right now* before it enters the shared cache.
    /// That check is not a formality: the whole point of this change is that a
    /// build can start while the repository still carries the *old* bytes of a
    /// same-version rebuild of a dependency, download those old bytes, and then
    /// finish after the new ones are live. Without re-validating at promote
    /// time, this job would restore the exact stale bytes the start-of-job
    /// reconcile removed.
    ///
    /// `None` means the check could not be performed (the DB fetch failed, which
    /// this method does not know about), so the promoted names are recorded as
    /// *unverified* and the next reconcile hashes them rather than trusting a
    /// stale cached value. A file is never silently blocked from promotion —
    /// the shared cache just stops assuming it is correct until proved.
    pub fn promote_job_pkgs(&self, label: &str, repo_db: Option<&RepoDb>) -> usize {
        let (Some(shared), Some(job)) = (self.pacman_pkg(), self.pacman_pkg_job(label)) else {
            return 0;
        };
        let Ok(read) = std::fs::read_dir(&job) else {
            return 0;
        };
        let mut promoted = 0;
        let mut promoted_names = Vec::new();
        for entry in read.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Only finished artifacts: `.part` files are interrupted downloads.
            if !is_package_artifact(name) {
                continue;
            }
            let path = entry.path();
            // Official-repository files are trusted as they are: they are not
            // in the aurcache DB to compare against, and nothing here can have
            // gone stale through the rename cycle.
            if let Some(want) = repo_db
                && let Some(entry) = want.get(name)
            {
                let matches = match std::fs::metadata(&path) {
                    Ok(meta) if meta.len() != entry.csize => false,
                    Ok(_) => sha256_file(&path).is_ok_and(|got| got == entry.sha256),
                    Err(_) => false,
                };
                if !matches {
                    tracing::info!(
                        "not promoting {name}: the repository no longer publishes these bytes"
                    );
                    continue;
                }
            }
            match std::fs::rename(&path, shared.join(name)) {
                Ok(()) => promoted += 1,
                Err(e) => {
                    tracing::warn!("could not promote {name} to the shared cache: {e}");
                    continue;
                }
            }
            promoted_names.push(name.to_string());
        }
        // What the DB vouches for is recorded as verified; whatever had to be
        // promoted without a DB to name it is recorded as *not* verified, so a
        // later reconcile of the same name hashes it from scratch.
        {
            let mut known = verified_lock();
            match repo_db {
                Some(want) => {
                    for name in &promoted_names {
                        if let Some(entry) = want.get(name) {
                            known.insert((shared.clone(), name.clone()), entry.sha256.clone());
                        }
                    }
                }
                None => {
                    for name in &promoted_names {
                        known.remove(&(shared.clone(), name.clone()));
                    }
                }
            }
        }
        promoted
    }

    /// Reconcile the shared package cache against what the served repository
    /// publishes, removing cache entries that no longer match.
    ///
    /// A cached AURCache archive and the `repo.db` entry that names it can only
    /// diverge inside this process: the chroots that use the cache mount it
    /// read-only, so every write goes through promote/evict/reconcile here. The
    /// one mode of divergence is a package deleted and re-added at the same
    /// version, whose rebuilt bytes differ from the still-cached bytes. pacman
    /// then finds the old file in a cache, skips its download, fails the
    /// integrity check at install because the file is stale, and cannot delete
    /// it from the read-only mount — the dependency install aborts.
    ///
    /// The pass is *event-driven*: a per-file record of the `repo.db` sha we
    /// last verified makes a no-op job cost a directory read and a hash-map
    /// lookup per package, with no stat and no hashing. A file is suspicious
    /// only when its record is missing or its entry changed, and even then a
    /// size mismatch removes it without reading a byte. Only files whose size
    /// still matches get hashed, in parallel once there are enough of them.
    ///
    /// Files the DB does not name (official core/extra/multilib packages) are
    /// left alone entirely: absence from the aurcache repo is not staleness.
    /// Returns how many stale archives were removed.
    pub fn reconcile_pkgs(&self, repo_db: &RepoDb) -> usize {
        let Some(shared) = self.pacman_pkg() else {
            return 0;
        };
        // Decided under one short hold, then stated outside it: `metadata` is
        // a syscall per entry and holding the process-global map across the
        // whole walk serialises every concurrent reconcile for no reason.
        // A concurrent reconcile can only duplicate a stat or a removal,
        // both idempotent, never corrupt the map.
        let todo: Vec<(String, RepoDbEntry)> = {
            let known = verified_lock();
            repo_db
                .iter()
                .filter(|(name, entry)| {
                    let key = (shared.clone(), (*name).clone());
                    !known.get(&key).is_some_and(|seen| *seen == entry.sha256)
                })
                .map(|(name, entry)| (name.clone(), entry.clone()))
                .collect()
        };
        let mut removed = 0;
        let mut suspects: Vec<Suspect> = Vec::new();
        let mut absent: Vec<Suspect> = Vec::new();
        let mut stale: Vec<String> = Vec::new();
        for (name, RepoDbEntry { csize, sha256: sha }) in todo {
            match std::fs::metadata(shared.join(&name)) {
                // Nothing cached; nothing to verify until some promote
                // actually lands the file.
                Err(_) => absent.push(Suspect { name, sha256: sha }),
                Ok(meta) if meta.len() != csize => {
                    if remove_cached(&shared, &name) {
                        removed += 1;
                    }
                    stale.push(name);
                }
                Ok(_) => suspects.push(Suspect { name, sha256: sha }),
            }
        }
        {
            let mut known = verified_lock();
            for suspect in absent {
                known.insert((shared.clone(), suspect.name), suspect.sha256);
            }
            for name in stale {
                known.remove(&(shared.clone(), name));
            }
            // Files no longer published are not consulted anymore; drop their
            // records so the map tracks the repository instead of growing.
            known.retain(|key, _| repo_db.contains_key(&key.1));
        }
        let verdicts = hash_suspects(&shared, &suspects);
        {
            let mut known = verified_lock();
            for Verdict { suspect, matches } in verdicts {
                let key = (shared.clone(), suspect.name.clone());
                if matches {
                    known.insert(key, suspect.sha256.clone());
                } else if remove_cached(&shared, &suspect.name) {
                    removed += 1;
                    known.remove(&key);
                }
            }
        }
        removed
    }

    /// Evict shared-cache packages over the package budget.
    ///
    /// Deliberately a separate pool from `SRCDEST`: the two differ by orders of
    /// magnitude in size and in refill cost, and one shared budget would let a
    /// few large VCS checkouts starve the package cache — every build then
    /// re-downloading its dependencies — or a big package cache evict sources
    /// that are expensive to re-clone.
    ///
    /// No `in_use` set is needed. Unlinking a file another chroot has open is
    /// safe on POSIX: the reader keeps its descriptor, and the worst case is a
    /// re-download.
    pub fn evict_pkgs(&self) -> Vec<String> {
        let Some(dir) = self.pacman_pkg() else {
            return Vec::new();
        };
        let entries = scan_pkgcache(&dir);
        let plan = plan_eviction(
            &entries,
            self.pkg_max_size,
            self.pkg_ttl,
            SystemTime::now(),
            &[],
        );
        for name in &plan {
            if let Err(e) = std::fs::remove_file(dir.join(name)) {
                tracing::warn!("could not evict cached package {name}: {e}");
            }
            // Detached signatures follow their package.
            let _ = std::fs::remove_file(dir.join(format!("{name}.sig")));
        }
        plan
    }

    /// Like [`Self::ensured`], but for a directory the build user reads rather
    /// than writes: `0755` instead of group-writable.
    fn ensured_readable(path: PathBuf) -> Option<PathBuf> {
        match std::fs::create_dir_all(&path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(e) =
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    {
                        tracing::debug!("could not set mode on {}: {e}", path.display());
                    }
                }
                Some(path)
            }
            Err(e) => {
                tracing::warn!("cache dir {} unavailable: {e}", path.display());
                None
            }
        }
    }

    /// Create a cache directory, group-writable.
    ///
    /// These directories are created by the worker but written by *builds*,
    /// which run as a different user (see `Config::build_user`). The parent is
    /// setgid so the group is inherited, but the mode is not: with a default
    /// umask the new directory would be `rwxr-xr-x` and owned by the worker,
    /// and devtools would fail with "You do not have write permission for the
    /// directory $SRCDEST". Group write is what bridges the two users.
    fn ensured(path: PathBuf) -> Option<PathBuf> {
        match std::fs::create_dir_all(&path) {
            Ok(()) => {
                Self::make_group_writable(&path);
                Some(path)
            }
            Err(e) => {
                tracing::warn!("cache dir {} unavailable: {e}", path.display());
                None
            }
        }
    }

    /// Best-effort `chmod g+rwx`. A failure is not fatal on its own: the
    /// directory may already be owned by the build user, in which case it is
    /// writable anyway.
    fn make_group_writable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let Ok(meta) = std::fs::metadata(path) else {
                return;
            };
            let mode = meta.permissions().mode();
            let wanted = mode | 0o070;
            if mode != wanted
                && let Err(e) =
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(wanted))
            {
                tracing::debug!("could not make {} group-writable: {e}", path.display());
            }
        }
    }

    /// Wipe a possibly-corrupt pkgbase source cache so the next build starts
    /// cold (self-heal). Best-effort.
    ///
    /// Takes with it the persistent build trees' checkouts of that cache, which
    /// cannot outlive it; see [`Self::wipe_borrowed_checkouts`].
    pub fn wipe_srcdest(&self, pkgbase: &str) {
        let path = self.root.join("srcdest").join(sanitize(pkgbase));
        if let Err(e) = self.remove_entry(&path) {
            tracing::warn!("could not wipe cache {}: {e}", path.display());
        }
        self.wipe_borrowed_checkouts(pkgbase);
    }

    /// Remove the checkouts in a pkgbase's persistent build trees whose objects
    /// lived in the `SRCDEST` mirror just wiped. Returns how many were removed.
    ///
    /// makepkg makes a VCS working copy with `git clone -s`, so the tree's only
    /// object store is the mirror, reached through
    /// `.git/objects/info/alternates`. The two caches are reclaimed
    /// independently and against very different budgets -- the mirrors against
    /// 10 GiB, the trees against 200 GiB -- so the mirror is always what goes
    /// first, leaving a checkout whose objects are gone. Usually nothing
    /// notices, because the next build re-clones the mirror and the fresh one
    /// holds everything the stale checkout still references.
    ///
    /// It does not when upstream rewrote a ref. `ttf-google-fonts-git` tracks
    /// `google/fonts`, whose `gh-pages` is a deploy branch force-pushed on
    /// every deploy: a checkout from six days earlier still had
    /// `refs/remotes/origin/gh-pages` at a commit that had been pushed over,
    /// which no fresh clone contains. makepkg's `git fetch` in that checkout
    /// then fails its connectivity check -- "fatal: bad object ... did not send
    /// all necessary objects" -- before the build starts, and identically on
    /// every retry, since nothing was clearing the checkout.
    ///
    /// Called both when the mirror is wiped (`wipe_srcdest`) and by the worker
    /// after a build dies with that signature, so a checkout left stale by an
    /// *earlier* mirror re-creation -- before this existed, say -- cannot hold
    /// a package's retries hostage either.
    ///
    /// Only the borrowed checkouts, never the tree around them: a checkout is a
    /// local clone of a mirror and costs seconds to make again, while the
    /// compiled output beside it is the hours a persistent tree exists to save,
    /// and it borrows nothing.
    pub(crate) fn wipe_borrowed_checkouts(&self, pkgbase: &str) -> usize {
        let root = self.root.join("builddir");
        let mut wiped = 0usize;
        for platform in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            if is_set_aside(&platform.file_name()) {
                continue;
            }
            // makepkg's own layout: `$BUILDDIR/$pkgbase/src/<repo>`.
            let src = platform.path().join(sanitize(pkgbase)).join("src");
            let Ok(entries) = std::fs::read_dir(&src) else {
                continue;
            };
            let mut removed = 0usize;
            for checkout in entries.flatten() {
                let path = checkout.path();
                if is_set_aside(&checkout.file_name()) || !borrows_from_srcdest(&path) {
                    continue;
                }
                match remove_tree(&path) {
                    Ok(()) => {
                        removed += 1;
                        tracing::info!(
                            "wiped {} with the source cache holding its objects",
                            path.display()
                        );
                    }
                    Err(e) => tracing::warn!("could not wipe {}: {e}", path.display()),
                }
            }
            // The tree is smaller than its stamp now says. Drop the stamp
            // rather than correct it: reclaim measures a tree without one, and
            // an overstated size makes it evict trees it did not need to.
            if removed > 0
                && let Some(name) = platform.file_name().to_str()
            {
                let _ = std::fs::remove_file(self.size_stamp(name, pkgbase));
            }
            wiped += removed;
        }
        wiped
    }

    /// Evict LRU source-cache entries above the size/TTL budget, skipping any
    /// pkgbase currently in use. Returns the pkgbases evicted.
    pub fn evict(&self, in_use: &[String]) -> Vec<String> {
        sweep_set_aside(&self.root.join("srcdest"));
        let entries = self.scan_srcdest();
        let plan = plan_eviction(&entries, self.max_size, self.ttl, SystemTime::now(), in_use);
        for pkgbase in &plan {
            self.wipe_srcdest(pkgbase);
            tracing::info!("evicted cache entry {pkgbase}");
        }
        plan
    }

    /// Best-effort scan of the source cache directory; an unreadable or
    /// missing directory yields no entries rather than an error.
    fn scan_srcdest(&self) -> Vec<CacheEntry> {
        let dir = self.root.join("srcdest");
        let mut entries = Vec::new();
        let Ok(read) = std::fs::read_dir(&dir) else {
            return entries;
        };
        for ent in read.flatten() {
            if !ent.file_type().is_ok_and(|t| t.is_dir()) || is_set_aside(&ent.file_name()) {
                continue;
            }
            let pkgbase = ent.file_name().to_string_lossy().into_owned();
            let path = ent.path();
            let size = self.entry_size(&path).or_else(|| measure_tree(&path));
            let last_used = ent
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            entries.push(CacheEntry {
                pkgbase,
                size,
                last_used,
            });
        }
        entries
    }
}

/// Remove a per-job directory, warning only if it is still there afterwards.
fn wipe_dir(path: &Path, what: &str) {
    if let Err(e) = std::fs::remove_dir_all(path)
        && path.exists()
    {
        tracing::warn!("could not wipe {what} {}: {e}", path.display());
    }
}

/// True for a finished package archive (not an in-flight `.part` download,
/// and not a detached signature, which is evicted with its package).
fn is_package_artifact(name: &str) -> bool {
    name.contains(".pkg.tar") && !name.ends_with(".part") && !name.ends_with(".sig")
}

/// Repository DB entries verified against the shared package cache:
/// `(cache dir, filename) -> last-verified `repo.db` sha256`.
///
/// Keyed on the cache dir as well as the name so the tests, which stack many
/// caches in one process, never see each other's records. The wrinkle this
/// records is narrow: the shared cache is written only by this worker
/// (chroots mount it read-only), so a cached AURCache file can only go stale
/// when its `repo.db` entry changes, and that a straightforward comparison
/// catches — no stat and no hashing for an unchanged file.
static VERIFIED: OnceLock<Mutex<HashMap<(PathBuf, String), String>>> = OnceLock::new();

fn verified_lock() -> MutexGuard<'static, HashMap<(PathBuf, String), String>> {
    VERIFIED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Many files by fiat; hashing this many without threads is the one case where
/// the added threads are plainly worth it.
const PARALLEL_HASH_THRESHOLD: usize = 8;

/// Hash each suspect archive and report whether it matches the `repo.db` sha.
///
/// The steady state is zero; a restart's first pass or a mass-`--from-installed`
/// rebuild re-verifies the whole repository at most once, and that is the only
/// case with enough files to pay for threads, so the work stays serial below
/// [`PARALLEL_HASH_THRESHOLD`] and fans out with `std::thread::scope` past it —
/// no channeling and no new dependency.
///
/// Each verdict comes back paired with the suspect it is about, rather than in
/// the suspects' order. A thread that panics loses its chunk's verdicts, and a
/// positional answer would then shift every later one onto the wrong file --
/// removing a good archive, or vouching for a stale one. Paired, a lost verdict
/// is only a file left unverified until the next pass.
fn hash_suspects<'a>(shared: &Path, suspects: &'a [Suspect]) -> Vec<Verdict<'a>> {
    let verdict = |suspect: &'a Suspect| Verdict {
        suspect,
        matches: sha256_file(&shared.join(&suspect.name)).is_ok_and(|got| got == suspect.sha256),
    };
    if suspects.len() < PARALLEL_HASH_THRESHOLD {
        return suspects.iter().map(verdict).collect();
    }
    let workers = std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .min(suspects.len());
    // Collected, not lazy: the handles must all exist before the first join,
    // or each thread is joined (and its chunk hashed) before the next spawns
    // and the "parallel" hashing is serial.
    #[allow(clippy::needless_collect)]
    std::thread::scope(|scope| {
        let threads: Vec<_> = suspects
            .chunks(suspects.len().div_ceil(workers))
            .map(|chunk| scope.spawn(move || chunk.iter().map(verdict).collect::<Vec<_>>()))
            .collect();
        threads
            .into_iter()
            .flat_map(|t| t.join().unwrap_or_default())
            .collect()
    })
}

/// A cached archive whose `repo.db` entry has not been verified against it.
struct Suspect {
    name: String,
    /// What `repo.db` says its sha256 is.
    sha256: String,
}

/// Whether a [`Suspect`]'s bytes hash to what `repo.db` says.
struct Verdict<'a> {
    suspect: &'a Suspect,
    matches: bool,
}

/// A package archive whose stored bytes no longer match its `repo.db` entry.
/// Removed, along with its detached signature. A file already gone counts as
/// removed; a genuine permission trouble surfaces as a warning and leaves the
/// file — and the manifest record that would have been cleared — in place, so
/// the next reconcile tries again.
fn remove_cached(shared: &Path, name: &str) -> bool {
    let gone = match std::fs::remove_file(shared.join(name)) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!("could not remove stale cached package {name}: {e}");
            false
        }
    };
    let _ = std::fs::remove_file(shared.join(format!("{name}.sig")));
    gone
}

/// Hex sha256 of a file, streamed through 64 KiB so multi-hundred-MB packages
/// are hashed without ever being in memory whole.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Parse `repo.db` (or `<name>.db`) — gzip'd tar of one `desc` per package, as
/// `repo-add` writes it — into a [`RepoDb`].
///
/// Each package's entry is a `<name>-<version>/desc` file holding
/// `%FILENAME%`, `%CSIZE%` and `%SHA256SUM%` fields. An entry missing any of
/// them is skipped rather than fatal; so is a tar member that is not a
/// two-level `desc`.
pub fn parse_repo_db(db: &[u8]) -> anyhow::Result<RepoDb> {
    use std::io::Read;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(db));
    let mut out = RepoDb::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let Ok(path) = entry.path() else { continue };
        if path.components().count() != 2
            || path.file_name().and_then(|n| n.to_str()) != Some("desc")
        {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        if let Some(desc) = parse_desc(&text) {
            out.insert(desc.filename, desc.entry);
        }
    }
    Ok(out)
}

/// `%FILENAME%`, and the `%CSIZE%` and `%SHA256SUM%` recorded for it, of one
/// `desc` file. A field is
/// one line plus one following value line, so values use the same `i+1` shape
/// as every other field parser in this repository.
fn parse_desc(desc: &str) -> Option<Desc> {
    // Peeking, not indexing: the value is the line after the field header,
    // and collecting every line only to index `i + 1` is one allocation per
    // package entry in every `repo.db` parse.
    let mut lines = desc.lines().peekable();
    let mut filename = None;
    let mut size = None;
    let mut sha = None;
    while let Some(line) = lines.next() {
        let value = lines.peek().map(|l| l.trim());
        match line {
            "%FILENAME%" => filename = value.filter(|v| !v.is_empty()).map(str::to_string),
            "%CSIZE%" => size = value.and_then(|v| v.trim().parse().ok()),
            "%SHA256SUM%" => sha = value.filter(|v| !v.is_empty()).map(str::to_string),
            _ => {}
        }
    }
    Some(Desc {
        filename: filename?,
        entry: RepoDbEntry {
            csize: size?,
            sha256: sha?,
        },
    })
}

/// One package's `desc` in a `repo.db`, as far as the cache needs it.
struct Desc {
    filename: String,
    entry: RepoDbEntry,
}

/// One cache entry per package file. `last_used` is the file's mtime, which for
/// a package is its *download* time — pacman does not touch it on a cache hit.
/// That is why the package pool defaults to size-bounded only: age would evict
/// a package used daily just for having been fetched a while ago.
fn scan_pkgcache(dir: &Path) -> Vec<CacheEntry> {
    let mut entries = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return entries;
    };
    for ent in read.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !is_package_artifact(&name) {
            continue;
        }
        let Ok(md) = ent.metadata() else { continue };
        entries.push(CacheEntry {
            pkgbase: name,
            size: Some(md.len()),
            last_used: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    entries
}

/// Replace path-unsafe characters so a pkgbase maps to a single directory.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Decide which entries to evict: everything older than `ttl` (when enabled),
/// then LRU entries until the total fits `max_size` (when enabled). Entries in
/// `in_use` are never evicted.
pub fn plan_eviction(
    entries: &[CacheEntry],
    max_size: u64,
    ttl: Duration,
    now: SystemTime,
    in_use: &[String],
) -> Vec<String> {
    // A set: the passes below test every entry against it, which is quadratic
    // over the slice.
    let in_use: std::collections::HashSet<&str> = in_use.iter().map(String::as_str).collect();
    let mut evict = Vec::new();
    let mut kept: Vec<&CacheEntry> = Vec::new();

    // Age-based eviction first.
    for e in entries {
        if in_use.contains(e.pkgbase.as_str()) {
            kept.push(e);
            continue;
        }
        let aged = !ttl.is_zero() && now.duration_since(e.last_used).is_ok_and(|age| age > ttl);
        if aged {
            evict.push(e.pkgbase.clone());
        } else {
            kept.push(e);
        }
    }

    // Size-based eviction: drop LRU survivors until within budget.
    if max_size > 0 {
        let mut total = kept
            .iter()
            .filter_map(|e| e.size)
            .fold(0_u64, u64::saturating_add);
        let mut unknown = kept.iter().filter(|e| e.size.is_none()).count();
        // Oldest first.
        kept.sort_by_key(|e| e.last_used);
        for e in kept {
            if unknown == 0 && total <= max_size {
                break;
            }
            if !in_use.contains(e.pkgbase.as_str()) {
                evict.push(e.pkgbase.clone());
                if let Some(size) = e.size {
                    total = total.saturating_sub(size);
                } else {
                    unknown -= 1;
                }
            }
        }
    }
    evict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs_ago: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - secs_ago)
    }
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
    }

    #[test]
    fn unknown_sizes_cannot_hide_an_over_budget_cache() {
        let entries = vec![
            CacheEntry {
                pkgbase: "active".into(),
                size: None,
                last_used: t(100),
            },
            CacheEntry {
                pkgbase: "idle".into(),
                size: Some(10),
                last_used: t(50),
            },
        ];
        assert_eq!(
            plan_eviction(&entries, 100, Duration::ZERO, now(), &["active".into()]),
            ["idle"]
        );
        assert!(plan_eviction(&entries, 0, Duration::ZERO, now(), &[]).is_empty());
    }

    #[test]
    fn evicts_aged_entries() {
        let entries = vec![
            CacheEntry {
                pkgbase: "old".into(),
                size: Some(10),
                last_used: t(100),
            },
            CacheEntry {
                pkgbase: "fresh".into(),
                size: Some(10),
                last_used: t(1),
            },
        ];
        let evict = plan_eviction(&entries, 0, Duration::from_secs(50), now(), &[]);
        assert_eq!(evict, vec!["old".to_string()]);
    }

    #[test]
    fn evicts_lru_over_size_budget() {
        let entries = vec![
            CacheEntry {
                pkgbase: "a".into(),
                size: Some(100),
                last_used: t(30),
            },
            CacheEntry {
                pkgbase: "b".into(),
                size: Some(100),
                last_used: t(20),
            },
            CacheEntry {
                pkgbase: "c".into(),
                size: Some(100),
                last_used: t(10),
            },
        ];
        // Budget 250 → must drop the oldest (a).
        let evict = plan_eviction(&entries, 250, Duration::ZERO, now(), &[]);
        assert_eq!(evict, vec!["a".to_string()]);
    }

    #[test]
    fn never_evicts_in_use() {
        let entries = vec![CacheEntry {
            pkgbase: "busy".into(),
            size: Some(1000),
            last_used: t(999),
        }];
        let evict = plan_eviction(
            &entries,
            10,
            Duration::from_secs(1),
            now(),
            &["busy".to_string()],
        );
        assert!(evict.is_empty());
    }

    #[test]
    fn disabled_budgets_evict_nothing() {
        let entries = vec![CacheEntry {
            pkgbase: "a".into(),
            size: Some(10_000),
            last_used: t(10_000),
        }];
        assert!(plan_eviction(&entries, 0, Duration::ZERO, now(), &[]).is_empty());
    }

    #[test]
    fn sanitizes_pkgbase() {
        assert_eq!(sanitize("ttf-google-fonts-git"), "ttf-google-fonts-git");
        assert_eq!(sanitize("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize("a/b"), "a_b");
    }

    /// Lay out one platform's persistent tree for `pkgbase`: a checkout whose
    /// objects are in the `SRCDEST` mirror, one that owns its objects, and one
    /// borrowing from somewhere else entirely.
    fn tree_with_checkouts(cache: &Cache, platform: &str, pkgbase: &str) -> PathBuf {
        let src = cache.builddir(platform).unwrap().join(pkgbase).join("src");
        for (name, alternates) in [
            ("borrowed", Some("/srcdest/fonts/objects\n")),
            ("owned", None),
            ("elsewhere", Some("/var/cache/other/objects\n")),
        ] {
            let git = src.join(name).join(".git").join("objects").join("info");
            std::fs::create_dir_all(&git).unwrap();
            if let Some(text) = alternates {
                std::fs::write(git.join("alternates"), text).unwrap();
            }
        }
        std::fs::create_dir_all(src.join("build-output")).unwrap();
        src
    }

    /// Wiping a source cache must take the checkouts that borrowed their
    /// objects from it: left behind, `git fetch` in one fails the next build
    /// outright once upstream has rewritten a ref the checkout still tracks.
    #[test]
    fn wiping_a_srcdest_takes_the_checkouts_that_borrowed_from_it() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Cache::new(tmp.path(), 0, 0, 0, 0);
        let srcdest = c.srcdest("fonts-git").unwrap();
        let src = tree_with_checkouts(&c, "x86_64", "fonts-git");
        c.write_size_stamp("x86_64", "fonts-git", 5_000);

        c.wipe_srcdest("fonts-git");

        assert!(!srcdest.exists(), "the source cache itself goes");
        assert!(
            !src.join("borrowed").exists(),
            "a checkout whose objects were in that cache cannot outlive it"
        );
        assert!(
            src.join("owned").exists() && src.join("build-output").exists(),
            "what does not borrow from the cache is the hours the tree exists to save"
        );
        assert!(
            src.join("elsewhere").exists(),
            "borrowing from another object store is not ours to wipe"
        );
        assert!(
            !c.size_stamp("x86_64", "fonts-git").exists(),
            "the tree is smaller than the stamp says; drop it so reclaim measures"
        );
    }

    /// Every platform's tree of that pkgbase, and no other package's.
    #[test]
    fn wiping_a_srcdest_reaches_every_platform_and_only_that_package() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Cache::new(tmp.path(), 0, 0, 0, 0);
        let x86 = tree_with_checkouts(&c, "x86_64", "fonts-git");
        let arm = tree_with_checkouts(&c, "aarch64", "fonts-git");
        let other = tree_with_checkouts(&c, "x86_64", "other-git");

        c.wipe_srcdest("fonts-git");

        assert!(!x86.join("borrowed").exists());
        assert!(
            !arm.join("borrowed").exists(),
            "downloads are shared by every platform, so every platform's tree borrowed from the one cache"
        );
        assert!(
            other.join("borrowed").exists(),
            "another package's checkout borrows from its own cache"
        );
    }

    /// The wipe says how many checkouts it took, so a caller can report whether
    /// the next build will re-clone from the mirror at all -- and calling it on
    /// a clean tree is a no-op, not an error.
    #[test]
    fn wiping_borrowed_checkouts_counts_what_it_took() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Cache::new(tmp.path(), 0, 0, 0, 0);
        let src = tree_with_checkouts(&c, "x86_64", "fonts-git");

        let n = c.wipe_borrowed_checkouts("fonts-git");
        assert_eq!(n, 1, "only the checkout borrowing from this SRCDEST goes");
        assert!(!src.join("borrowed").exists());
        assert!(
            src.join("owned").exists() && src.join("build-output").exists(),
            "the compiled tree beside the checkout is preserved"
        );
        assert_eq!(
            c.wipe_borrowed_checkouts("fonts-git"),
            0,
            "nothing left to take is not an error"
        );
    }

    /// The parent of a borrowed checkout is owned by the build user (builder)
    /// with 0755, so the worker (aurcache) cannot `rename` the entry out.
    /// `remove_tree` must fall back to `privileged_rm` rather than leaving
    /// `wipe_borrowed_checkouts` logging "Permission denied" on every retry.
    /// This is the `ttf-google-fonts-git` symptom: `gh-pages` force-pushed
    /// away leaves `git fetch` failing with "bad object" on every build until
    /// the checkout is gone, and the previous code never removed it.
    #[test]
    fn remove_tree_falls_back_to_privileged_rm_when_parent_not_writable() {
        use std::os::unix::fs::PermissionsExt;

        // Skip when running as root: root bypasses permission checks, so the
        // rename would succeed without needing the fallback, and the test
        // would not exercise the branch it is meant to cover.
        if unsafe { libc::getuid() } == 0 {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let child = parent.join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("file"), b"data").unwrap();

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = remove_tree(&child);

        // Restore so the tempdir can be cleaned even if the test fails.
        let _ = std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755));

        // When `sudo -n rm` is available (as on workers and on this host) the
        // fallback succeeds; otherwise it returns an error that must mention
        // the sudo attempt so the branch is observable.
        if let Err(e) = result {
            assert!(
                e.to_string().contains("and with sudo"),
                "should have tried privileged_rm: {e}"
            );
        } else {
            assert!(
                !child.exists(),
                "privileged fallback should have removed the tree"
            );
        }
    }
}

#[cfg(test)]
mod pkgcache_tests {
    use super::*;

    fn cache(dir: &Path, pkg_max: u64) -> Cache {
        Cache::new(dir, 0, 0, pkg_max, 0)
    }

    fn write(path: &Path, bytes: usize) {
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    /// Discarding takes the tree and its size stamp, and says whether there
    /// was one: a build that failed before its tree existed discards nothing.
    #[test]
    fn a_discarded_tree_and_its_stamp_are_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();
        let tree = root.join("gone");
        std::fs::create_dir_all(tree.join("src/deep")).unwrap();
        write(&tree.join("src/deep/blob"), 10);
        c.write_size_stamp("x86_64", "gone", 1000);

        assert_eq!(c.discard_builddir("x86_64", "gone"), Discard::Removed);
        assert!(!tree.exists(), "the tree goes");
        assert!(
            !c.size_stamp("x86_64", "gone").exists(),
            "its stamp goes with it"
        );
        assert_eq!(
            c.discard_builddir("x86_64", "never-there"),
            Discard::Absent,
            "nothing there, nothing discarded"
        );

        // A tree a keep already took leaves its stamp behind: it goes too.
        c.write_size_stamp("x86_64", "taken", 1000);
        assert_eq!(c.discard_builddir("x86_64", "taken"), Discard::Absent);
        assert!(!c.size_stamp("x86_64", "taken").exists());
    }

    /// Every other cache directory is group-writable so the build user can
    /// write it. This one must not be: the build reads its own trust store and
    /// has no business rewriting it.
    #[test]
    fn a_keyring_replica_is_not_writable_by_the_build() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let replica = c.gnupg_job("job-7").unwrap();
        let shared = c.gnupg_home().unwrap();

        let mode = std::fs::metadata(&replica).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o022,
            0,
            "the replica must not be writable: {mode:o}"
        );
        assert_ne!(
            std::fs::metadata(&shared).unwrap().permissions().mode() & 0o020,
            0,
            "the shared keyring stays group-writable for the worker"
        );
    }

    /// A replica belongs to one build and goes away with it.
    #[test]
    fn a_keyring_replica_is_wiped_with_its_job() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let replica = c.gnupg_job("job-7").unwrap();
        std::fs::write(replica.join("pubring.kbx"), b"keys").unwrap();

        c.wipe_gnupg_job("job-7");

        assert!(!replica.exists());
        assert!(
            c.gnupg_home().unwrap().exists(),
            "wiping a replica must not touch the shared keyring"
        );
    }

    /// Least recently used goes first, and the tree the build is about to use
    /// is never evicted -- even when it is itself what breaches the cap, since
    /// dropping it would defeat the point of keeping trees at all.
    #[test]
    fn reclaim_evicts_oldest_but_never_the_one_in_use() {
        use std::time::{Duration, SystemTime};

        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();

        for (name, age) in [("old", 300), ("mid", 200), ("wanted", 100)] {
            let tree = root.join(name);
            std::fs::create_dir_all(&tree).unwrap();
            c.write_size_stamp("x86_64", name, 100);
            let when = SystemTime::now() - Duration::from_secs(age);
            std::fs::File::open(&tree)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }

        // Room for one tree, so two must go -- but "wanted" is in use.
        c.reclaim_builddirs("x86_64", &["wanted".to_string()], 150, 0);

        assert!(!root.join("old").exists(), "oldest goes first");
        assert!(!root.join("mid").exists(), "then the next oldest");
        assert!(
            root.join("wanted").exists(),
            "the tree this build needs must survive"
        );
    }

    /// A sibling build's tree is in use too, however old it looks: a long
    /// compile does not touch its tree's root, and deleting it mid-build fails
    /// that build.
    #[test]
    fn reclaim_spares_every_tree_a_running_build_uses() {
        use std::time::{Duration, SystemTime};

        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();

        for (name, age) in [("sibling", 300), ("idle", 200), ("mine", 100)] {
            let tree = root.join(name);
            std::fs::create_dir_all(&tree).unwrap();
            c.write_size_stamp("x86_64", name, 100);
            let when = SystemTime::now() - Duration::from_secs(age);
            std::fs::File::open(&tree)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }

        c.reclaim_builddirs(
            "x86_64",
            &["mine".to_string(), "sibling".to_string()],
            150,
            0,
        );

        assert!(root.join("sibling").exists(), "a running build's tree went");
        assert!(root.join("mine").exists());
        assert!(!root.join("idle").exists(), "the idle tree pays instead");
    }

    /// Room to spare means nothing is touched, however old. Reclaiming disk
    /// nothing else wants would trade a rebuild for no gain -- and it is what
    /// makes the tree of a deleted package cost nothing until the space is
    /// actually needed.
    #[test]
    fn nothing_is_evicted_while_there_is_room() {
        use std::time::{Duration, SystemTime};

        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();

        let tree = root.join("ancient");
        std::fs::create_dir_all(&tree).unwrap();
        c.write_size_stamp("x86_64", "ancient", 100);
        let when = SystemTime::now() - Duration::from_secs(365 * 24 * 60 * 60);
        std::fs::File::open(&tree)
            .unwrap()
            .set_modified(when)
            .unwrap();

        // A cap nothing comes close to.
        c.reclaim_builddirs("x86_64", &[], u64::MAX, 0);

        assert!(
            tree.exists(),
            "an unused tree is not worth reclaiming while there is room for it"
        );
    }

    /// A tree with no stamp is measured rather than counted as free.
    #[test]
    fn an_unstamped_tree_is_measured() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();
        let tree = root.join("unstamped");
        std::fs::create_dir_all(tree.join("deep")).unwrap();
        write(&tree.join("deep").join("blob"), 4096);

        assert_eq!(c.stamped_size("x86_64", &tree), Some(4096));
        // And it is stamped, so the walk happens once -- beside the tree, which
        // the build user owns, not inside it.
        assert_eq!(
            std::fs::read_to_string(c.size_stamp("x86_64", "unstamped")).unwrap(),
            "4096"
        );
        assert!(!tree.join(".aurcache-size").exists());
    }

    /// A walk that cannot read part of a tree says so, rather than reporting
    /// the readable part as the whole: makepkg leaves `pkg/` mode 111.
    #[test]
    fn a_walk_that_cannot_see_everything_is_not_complete() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("tree");
        std::fs::create_dir_all(tree.join("pkg")).unwrap();
        write(&tree.join("pkg").join("blob"), 4096);
        write(&tree.join("readable"), 100);
        assert_eq!(
            aurcache_common::fs::measure_directory(&tree),
            aurcache_common::fs::DirectoryMeasurement::Complete { bytes: 4196 }
        );

        std::fs::set_permissions(tree.join("pkg"), std::fs::Permissions::from_mode(0o111)).unwrap();
        let unreadable = std::fs::read_dir(tree.join("pkg")).is_err();
        let measured = aurcache_common::fs::measure_directory(&tree);
        std::fs::set_permissions(tree.join("pkg"), std::fs::Permissions::from_mode(0o755)).unwrap();
        // Root reads everything, so the case only exists for a normal user.
        if unreadable {
            assert_eq!(
                measured,
                aurcache_common::fs::DirectoryMeasurement::Incomplete { known_bytes: 100 }
            );
        }
    }

    /// A reclaimed tree leaves nothing under its own name, and nothing set
    /// aside once the removal succeeds; its stamp goes with it.
    #[test]
    fn a_reclaimed_tree_is_set_aside_then_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();
        let tree = root.join("gone");
        std::fs::create_dir_all(tree.join("src/deep")).unwrap();
        write(&tree.join("src/deep/blob"), 10);
        c.write_size_stamp("x86_64", "gone", 1000);

        c.reclaim_builddirs("x86_64", &[], 1, 0);

        assert!(!tree.exists());
        assert!(!c.size_stamp("x86_64", "gone").exists());
        let left: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    /// Something an earlier pass set aside and could not delete is neither a
    /// tree to count nor a source cache entry, and the next pass finishes it.
    #[test]
    fn a_tree_left_set_aside_is_swept_and_never_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);

        let root = c.builddir("x86_64").unwrap();
        let aside = root.join(".old.aurcache-removing");
        std::fs::create_dir_all(aside.join("src")).unwrap();
        // Would breach any budget if it were counted as a tree.
        c.reclaim_builddirs("x86_64", &[], 1, 0);
        assert!(!aside.exists());

        let srcdest = c.srcdest("pkg").unwrap();
        let src_aside = srcdest.with_file_name(".pkg.aurcache-removing");
        std::fs::create_dir_all(&src_aside).unwrap();
        assert!(
            c.scan_srcdest()
                .iter()
                .all(|e| !e.pkgbase.ends_with(SET_ASIDE_SUFFIX))
        );
        c.evict(&[]);
        assert!(!src_aside.exists());
    }

    /// Removing a name whose earlier removal did not finish still works: the
    /// leftover would otherwise block the rename.
    #[test]
    fn removal_is_not_blocked_by_an_unfinished_earlier_one() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("pkg");
        std::fs::create_dir_all(tree.join("a")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".pkg.aurcache-removing/b")).unwrap();

        remove_tree(&tree).unwrap();

        assert!(!tree.exists());
        assert!(!tmp.path().join(".pkg.aurcache-removing").exists());
        remove_tree(&tree).unwrap();
    }

    #[test]
    fn only_finished_archives_are_promoted() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let job = c.pacman_pkg_job("job-1").unwrap();
        write(&job.join("foo-1.0-1-x86_64.pkg.tar.zst"), 10);
        // An interrupted download must not be published as a real package.
        write(&job.join("bar-2.0-1-x86_64.pkg.tar.zst.part"), 10);

        assert_eq!(c.promote_job_pkgs("job-1", None), 1);
        let shared = c.pacman_pkg().unwrap();
        assert!(shared.join("foo-1.0-1-x86_64.pkg.tar.zst").exists());
        assert!(!shared.join("bar-2.0-1-x86_64.pkg.tar.zst.part").exists());
        // The partial stays behind and goes with the job directory.
        assert!(job.join("bar-2.0-1-x86_64.pkg.tar.zst.part").exists());
    }

    /// Two jobs downloading the same dependency both promote it. The second
    /// overwrites the first with identical content; `rename` makes that atomic,
    /// so no reader can observe a half-written file.
    #[test]
    fn promoting_the_same_package_twice_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        for label in ["job-1", "job-2"] {
            let job = c.pacman_pkg_job(label).unwrap();
            write(&job.join("cmake-1.0-1-x86_64.pkg.tar.zst"), 10);
            assert_eq!(c.promote_job_pkgs(label, None), 1);
        }
        assert!(
            c.pacman_pkg()
                .unwrap()
                .join("cmake-1.0-1-x86_64.pkg.tar.zst")
                .exists()
        );
    }

    #[test]
    fn evicts_packages_over_the_package_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 150);
        let shared = c.pacman_pkg().unwrap();
        write(&shared.join("a-1.0-1-x86_64.pkg.tar.zst"), 100);
        write(&shared.join("b-1.0-1-x86_64.pkg.tar.zst"), 100);

        let evicted = c.evict_pkgs();
        assert_eq!(evicted.len(), 1, "one package should be dropped");
    }

    /// The package budget is separate from the source budget on purpose: a
    /// shared one lets large VCS checkouts starve the package cache.
    #[test]
    fn source_budget_does_not_evict_packages() {
        let tmp = tempfile::tempdir().unwrap();
        // Source budget of 1 byte, package budget generous.
        let c = Cache::new(tmp.path(), 1, 0, 10_000, 0);
        let shared = c.pacman_pkg().unwrap();
        write(&shared.join("a-1.0-1-x86_64.pkg.tar.zst"), 100);

        assert!(c.evict_pkgs().is_empty());
        assert!(shared.join("a-1.0-1-x86_64.pkg.tar.zst").exists());
    }

    #[test]
    fn signatures_follow_their_package() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 1);
        let shared = c.pacman_pkg().unwrap();
        write(&shared.join("a-1.0-1-x86_64.pkg.tar.zst"), 100);
        write(&shared.join("a-1.0-1-x86_64.pkg.tar.zst.sig"), 10);

        c.evict_pkgs();
        assert!(!shared.join("a-1.0-1-x86_64.pkg.tar.zst").exists());
        assert!(!shared.join("a-1.0-1-x86_64.pkg.tar.zst.sig").exists());
    }

    fn sha_of(bytes: &[u8]) -> String {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(bytes))
    }

    fn make_db(entries: &[(&str, u64, &str)]) -> RepoDb {
        entries
            .iter()
            .map(|&(name, csize, sha)| {
                let entry = RepoDbEntry {
                    csize,
                    sha256: sha.to_string(),
                };
                (name.to_string(), entry)
            })
            .collect()
    }

    /// A real `repo-add`-shaped `repo.db`: gzip'd tar of one `desc` per package.
    fn repo_db_bytes(entries: &[(&str, u64, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut tar = tar::Builder::new(Vec::new());
        for (name, size, sha) in entries {
            let mut header = tar::Header::new_gnu();
            let data = format!("%FILENAME%\n{name}\n%CSIZE%\n{size}\n%SHA256SUM%\n{sha}\n");
            header.set_size(data.len() as u64);
            tar.append_data(&mut header, format!("{name}-1-any/desc"), data.as_bytes())
                .unwrap();
        }
        let raw = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn parses_a_repo_db_into_names_sizes_and_shas() {
        let name_a = "a-1.0-1-x86_64.pkg.tar.zst";
        let bytes = repo_db_bytes(&[
            (name_a, 100, &sha_of(&[b'x'; 100])),
            ("b-2.0-1-x86_64.pkg.tar.zst", 42, &"f".repeat(64)),
        ]);
        let repo = parse_repo_db(&bytes).unwrap();
        assert_eq!(repo.len(), 2);
        let entry = |csize, sha256| RepoDbEntry { csize, sha256 };
        assert_eq!(repo.get(name_a), Some(&entry(100, sha_of(&[b'x'; 100]))));
        assert_eq!(
            repo.get("b-2.0-1-x86_64.pkg.tar.zst"),
            Some(&entry(42, "f".repeat(64)))
        );
    }

    /// The canonical failure this all exists for: a cache entry between two
    /// runs of the same-version rebuild, whose size already betrays it. The
    /// size check removes it without hashing a byte.
    #[test]
    fn reconcile_removes_a_file_whose_entry_grew_in_size() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&shared.join(name), 100);
        write(&shared.join(format!("{name}.sig")), 10);

        let repo = make_db(&[(name, 120, &"f".repeat(64))]);
        assert_eq!(c.reconcile_pkgs(&repo), 1);
        assert!(!shared.join(name).exists());
        assert!(!shared.join(format!("{name}.sig")).exists());
    }

    /// Same size, new bytes — the sizing trick cannot see it, so the file is
    /// hashed before it is trusted again.
    #[test]
    fn reconcile_hashes_and_removes_a_file_whose_bytes_changed_at_the_same_size() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&shared.join(name), 100);

        let repo = make_db(&[(name, 100, &sha_of(&[b'y'; 100]))]);
        assert_eq!(c.reconcile_pkgs(&repo), 1);
        assert!(!shared.join(name).exists());
    }

    /// Enough suspects to hash on threads, with good and stale files
    /// interleaved: every verdict lands on its own file.
    #[test]
    fn a_parallel_reconcile_judges_each_file_by_its_own_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let shared = c.pacman_pkg().unwrap();
        let mut entries = Vec::new();
        for i in 0..(PARALLEL_HASH_THRESHOLD * 2) {
            let name = format!("p{i}-1.0-1-x86_64.pkg.tar.zst");
            write(&shared.join(&name), 100);
            // Odd files were rebuilt: same size, different bytes.
            let sha = if i % 2 == 0 {
                sha_of(&[b'x'; 100])
            } else {
                sha_of(&[b'y'; 100])
            };
            entries.push((name, sha));
        }
        let repo: RepoDb = entries
            .iter()
            .map(|(name, sha)| {
                let entry = RepoDbEntry {
                    csize: 100,
                    sha256: sha.clone(),
                };
                (name.clone(), entry)
            })
            .collect();

        assert_eq!(c.reconcile_pkgs(&repo), PARALLEL_HASH_THRESHOLD);
        for (i, (name, _)) in entries.iter().enumerate() {
            assert_eq!(shared.join(name).exists(), i % 2 == 0, "{name}");
        }
    }

    /// A matching file costs a directory read and a map lookup, not a hash, and
    /// a repeat pass is a no-op.
    #[test]
    fn reconcile_leaves_matching_files_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&shared.join(name), 100);

        let repo = make_db(&[(name, 100, &sha_of(&[b'x'; 100]))]);
        assert_eq!(c.reconcile_pkgs(&repo), 0);
        assert!(shared.join(name).exists());
        assert_eq!(c.reconcile_pkgs(&repo), 0);
    }

    /// Official core/extra/multilib archives are absent from the aurcache DB
    /// *by construction*; absence is not staleness, so they are left alone.
    #[test]
    fn reconcile_leaves_files_the_db_does_not_name_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let shared = c.pacman_pkg().unwrap();
        let official = "gcc-13.2.1-3-x86_64.pkg.tar.zst";
        write(&shared.join(official), 50);

        assert_eq!(c.reconcile_pkgs(&RepoDb::new()), 0);
        assert!(shared.join(official).exists());
    }

    /// A job that fetched the *old* bytes of a same-version rebuild must not be
    /// able to restore them over the read-only cache: the DB names them, the
    /// size prefilter catches them, and the stale bytes never reach the shared
    /// side at all.
    #[test]
    fn promote_refuses_old_bytes_of_a_rebuilt_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let job = c.pacman_pkg_job("job-1").unwrap();
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&job.join(name), 100);

        let repo = make_db(&[(name, 100, &sha_of(&[b'y'; 100]))]);
        assert_eq!(c.promote_job_pkgs("job-1", Some(&repo)), 0);
        assert!(job.join(name).exists(), "the artifact must stay put");
        assert!(!shared.join(name).exists());
    }

    /// An artifact that passes the check (size and hash) is promoted and
    /// recorded as verified, so the very next reconcile costs nothing.
    #[test]
    fn promote_with_a_matching_db_records_verified() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let job = c.pacman_pkg_job("job-1").unwrap();
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&job.join(name), 100);

        let repo = make_db(&[(name, 100, &sha_of(&[b'x'; 100]))]);
        assert_eq!(c.promote_job_pkgs("job-1", Some(&repo)), 1);
        assert!(shared.join(name).exists());
        assert_eq!(c.reconcile_pkgs(&repo), 0);
    }

    /// The end-to-end story: the DB fetch failed, so a job's stale old bytes
    /// were promoted unvalidated; the manifest records that, and the reconcile
    /// that sees the rebuilt DB re-hashes and removes them before pacman can
    /// pick them up from the read-only mount.
    #[test]
    fn an_unvalidated_promote_is_rechecked_by_the_next_reconcile() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let job = c.pacman_pkg_job("job-1").unwrap();
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&job.join(name), 100);

        // No DB (the fetch failed): the promote cannot tell old from new bytes.
        assert_eq!(c.promote_job_pkgs("job-1", None), 1);
        assert!(shared.join(name).exists());

        // The rebuilt DB says these bytes are no longer what it publishes.
        let repo = make_db(&[(name, 100, &sha_of(&[b'y'; 100]))]);
        assert_eq!(c.reconcile_pkgs(&repo), 1);
        assert!(!shared.join(name).exists());
    }
}

/// Free bytes on the filesystem holding `path`.
#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and outlives the call; `stat` is a
    // valid out-parameter.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &raw mut stat) };
    (rc == 0).then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// Measure a tree completely, using the existing privileged fallback if needed.
/// No partial result is written to a stamp or reported as a known total.
fn measure_tree(path: &Path) -> Option<u64> {
    aurcache_common::fs::measure_directory(path)
        .total()
        .or_else(|| privileged_du(path))
}

/// `du` as root, for a tree the worker cannot read all of.
///
/// `-n` so a host without the sudoers grant fails at once instead of waiting on
/// a password prompt no one will answer.
fn privileged_du(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("sudo")
        .args(["-n", "du", "-s", "--bytes", "--one-file-system", "--"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        tracing::debug!(
            "sudo du {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// What a tree is renamed to while it is being removed.
const SET_ASIDE_SUFFIX: &str = ".aurcache-removing";

/// Whether a directory entry is a tree set aside for removal, rather than a
/// cache entry of its own.
fn is_set_aside(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().ends_with(SET_ASIDE_SUFFIX)
}

/// Where devtools binds `SRCDEST` inside every build's chroot, and so the path
/// a checkout borrowing from it records.
const CHROOT_SRCDEST: &str = "/srcdest/";

/// Whether a checkout's objects live in the `SRCDEST` mirror rather than in
/// itself.
///
/// `git clone -s` records the mirror it borrows from in `alternates`, as the
/// absolute path it was cloned by -- which is the in-chroot one, since that is
/// where the clone ran. A checkout borrowing from anywhere else, or from
/// nowhere, owns its objects and is none of our business.
fn borrows_from_srcdest(checkout: &Path) -> bool {
    // Non-bare first, which is what makepkg makes; the bare spelling costs one
    // failed `read_to_string` to also cover a PKGBUILD that made its own.
    [checkout.join(".git"), checkout.to_path_buf()]
        .iter()
        .filter_map(|git| std::fs::read_to_string(git.join("objects/info/alternates")).ok())
        .any(|alternates| {
            alternates
                .lines()
                .any(|line| line.trim().starts_with(CHROOT_SRCDEST))
        })
}

/// Remove a cache tree the build user may have written into.
///
/// Such a tree is not the worker's to delete: makepkg and the build write it
/// as the build user, mode 755, and the worker can only unlink what sits
/// directly in a directory it may write. `remove_dir_all` as the worker failed
/// on every build tree ("Permission denied") -- none was ever reclaimed -- and
/// on source caches it failed *partway*, taking the tarballs next to a git
/// checkout it could not touch.
///
/// So the tree is renamed aside first, which needs only the parent directory
/// the worker owns. Nothing is ever left half-deleted under the name makepkg
/// looks for -- a partial build tree reads as resumable, a partial checkout as
/// something to update. Then it is deleted: as the worker where that is enough,
/// otherwise with `sudo rm`, as the chroot operations already are. A removal
/// that fails even so stays set aside, never counted as an entry, and is
/// finished by [`sweep_set_aside`] on a later pass.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    let Some(name) = path.file_name() else {
        return Err(std::io::Error::other("no file name to set aside"));
    };
    let aside = path.with_file_name(format!(".{}{SET_ASIDE_SUFFIX}", name.to_string_lossy()));
    // An earlier removal of the same name that did not finish would make the
    // rename fail on a non-empty target.
    if aside.exists() {
        remove_set_aside(&aside)?;
    }
    match std::fs::rename(path, &aside) {
        Ok(()) => remove_set_aside(&aside),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            // The parent belongs to the build user (0755), so the worker
            // cannot rename the entry out of it. Remove it in place with
            // privilege instead, as `remove_set_aside` already does for the
            // `rmdir` half -- otherwise a stale checkout in a persistent tree
            // survives every wipe and fails every retry identically.
            privileged_rm(path).map_err(|with_sudo| {
                std::io::Error::new(e.kind(), format!("{e}; and with sudo: {with_sudo}"))
            })
        }
        Err(e) => Err(e),
    }
}

/// Delete a tree already renamed aside.
fn remove_set_aside(aside: &Path) -> std::io::Result<()> {
    let Err(e) = std::fs::remove_dir_all(aside) else {
        return Ok(());
    };
    match e.kind() {
        std::io::ErrorKind::NotFound => Ok(()),
        std::io::ErrorKind::PermissionDenied => privileged_rm(aside).map_err(|with_sudo| {
            std::io::Error::new(e.kind(), format!("{e}; and with sudo: {with_sudo}"))
        }),
        _ => Err(e),
    }
}

/// `rm -rf` as root. `--one-file-system`, as devtools removes chroot copies: a
/// bind mount left behind under a tree must not be walked into.
fn privileged_rm(path: &Path) -> Result<(), String> {
    let out = std::process::Command::new("sudo")
        .args(["-n", "rm", "-rf", "--one-file-system", "--"])
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() && !path.exists() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Finish removing whatever an earlier pass set aside and could not delete.
fn sweep_set_aside(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if !is_set_aside(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        if let Err(e) = remove_set_aside(&path) {
            tracing::warn!("could not remove {}: {e}", path.display());
        }
    }
}
