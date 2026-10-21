use std::collections::BTreeMap;
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alpm_srcinfo::SourceInfoV1;
use aurcache_common::api::package::SourceFileContent;
use aurcache_db::packages::SourceData;
use git2::Oid;
use lru::LruCache;
use sea_orm::DatabaseConnection;
use tokio::sync::Mutex;

use crate::git::checkout::EmptyRepository;
use crate::patch::SourcePatch;
use crate::pkgbuild::fix_source_urls;

/// Base URL for AUR git repositories. AUR packages are unified with git
/// sources: `https://aur.archlinux.org/{pkgbase}.git`, ref `HEAD`, no
/// subfolder. The AUR RPC (`AurClient`) is only used for metadata/dependency
/// resolution, not for fetching sources.
///
/// `AUR_GIT_URL` points it elsewhere, as `AUR_RPC_URL` does the RPC: a local
/// mirror, or the repositories `scripts/showcase/restore.py` serves so a
/// restored instance reads its sources without the network.
fn default_aur_git_base_url() -> String {
    std::env::var("AUR_GIT_URL").unwrap_or_else(|_| "https://aur.archlinux.org".to_string())
}

/// Git's per-checkout metadata directory, excluded from source archives.
const GIT_METADATA_DIR: &str = ".git";

/// Maximum number of distinct sources kept in the in-memory cache at once.
/// At roughly 5-20KB per entry (a small tar.gz of PKGBUILD/.SRCINFO/aux
/// files, not the package's actual upstream source code, plus a parsed
/// `.SRCINFO`), this bounds worst-case memory to a low tens-of-MB even if
/// every package were touched in a single sweep (e.g. periodic version
/// checks), while comfortably covering the working set of packages actively
/// being added/built/edited at once.
const CACHE_CAPACITY: usize = 1000;

/// A single rendered view of a source: either the pristine upstream fetch,
/// or the result of applying a patch on top of it.
#[derive(Clone)]
struct SourceSnapshot {
    archive_bytes: Vec<u8>,
    /// `None` when this snapshot's `.SRCINFO`/`PKGBUILD` could not be parsed
    /// (e.g. a malformed PKGBUILD upstream, such as `ogdf`). Browsing and
    /// editing raw source files must keep working in that case so a patch
    /// can be authored to fix the parse failure before the source is ever
    /// added as a package; only dependency/version resolution actually
    /// requires a successfully parsed `.SRCINFO`.
    sourceinfo: Option<SourceInfoV1>,
    /// Best-effort pkgbase, used to name the archive's top-level directory
    /// and as the file-listing root even when `sourceinfo` is `None`.
    pkgbase: String,
}

impl SourceSnapshot {
    /// The parsed `.SRCINFO`, or why there is none.
    fn parsed(&self) -> anyhow::Result<SourceInfoV1> {
        self.sourceinfo
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Source's .SRCINFO/PKGBUILD could not be parsed"))
    }

    /// The `PKGBUILD`'s text.
    fn pkgbuild(&self) -> anyhow::Result<String> {
        read_file_from_archive(&self.archive_bytes, &self.pkgbase, "PKGBUILD")
    }
}

/// What [`SnapshotStore::refresh`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// The source moved, or was not cached yet: the entry was rebuilt.
    Changed,
    /// The cached entry was already current.
    Current,
}

/// A source's parsed `.SRCINFO`, its `PKGBUILD` and the metadata read beside
/// them.
pub struct Resolved {
    pub sourceinfo: SourceInfoV1,
    pub pkgbuild: String,
    pub metadata: crate::package::source_metadata::SourceMetadata,
}

struct CacheEntry {
    /// Resolved commit id of the raw (unpatched) checkout, used to detect
    /// whether a `refresh` actually changed anything.
    commit: Oid,
    /// What most callers want: the patched result if a patch is currently
    /// active, otherwise identical to the raw fetch. There is at most one
    /// active patch per source at any time (matching the data model - a
    /// package has a single `patch` column), so a new/cleared patch simply
    /// replaces this entry rather than accumulating variants.
    active: SourceSnapshot,
    /// Only `Some` when a patch is currently active, holding the pristine
    /// pre-patch snapshot. Needed so `read_file(patch=None)` can diff edits
    /// against the true original rather than the currently-patched content.
    /// When no patch is active this is `None` (rather than a redundant copy
    /// of `active`), since a caller wanting the original in that case can
    /// just use `active` directly.
    original: Option<SourceSnapshot>,
    /// The patch that produced `active` from `original`, if any. Kept so
    /// `refresh` can re-apply the same patch on top of a freshly fetched
    /// raw source instead of silently dropping it.
    patch: Option<SourcePatch>,
    /// The checkout's commit history, read the first time anyone asks: it is
    /// the same until `commit` moves, which replaces this entry.
    history: tokio::sync::OnceCell<PackagingHistory>,
}

impl CacheEntry {
    /// The pristine (pre-patch) snapshot: `original` if a patch is active,
    /// otherwise `active` itself (which already *is* the pristine content).
    fn original(&self) -> &SourceSnapshot {
        self.original.as_ref().unwrap_or(&self.active)
    }
}

/// Directory under which persistent git checkouts are kept, one subdirectory
/// per `SourceData::cache_key()`. Reusing these clones across calls means a
/// `refresh` only needs to `git fetch` (transfer new objects) instead of a
/// full re-clone.
pub(crate) fn default_checkout_root() -> PathBuf {
    std::env::var("AURCACHE_SOURCE_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./source_cache"))
}

/// Cache for checked-out source repositories and their parsed `.SRCINFO`.
///
/// A single instance should be passed around so that repeated requests for
/// the same source return the cached result. Backed by a persistent on-disk
/// git checkout per source (see `default_checkout_root`), so repeat fetches
/// are cheap incremental `git fetch`s rather than full clones/downloads.
///
/// Patched content is never written to disk - only the persistent raw git
/// checkout is - a patch is applied in a temporary directory that's deleted
/// once the resulting archive/`.SRCINFO` have been computed, with the result
/// held only in this in-memory cache. Applying a patch on top of an
/// already-fetched raw checkout is cheap (in-memory extract + patch + re-tar,
/// no network), so a cache miss on the patched content never requires
/// re-fetching from the remote.
///
/// Bounded by an LRU eviction policy (see [`CACHE_CAPACITY`]) so memory use
/// doesn't grow unboundedly as more distinct sources are touched over the
/// process's lifetime.
pub struct SnapshotStore {
    /// Keyed by `SourceData::cache_key()`. At most one entry per source.
    cache: Mutex<LruCache<String, Arc<CacheEntry>>>,
    /// One lock per checkout directory, by its name under `checkout_root`.
    ///
    /// Git cannot share a working tree between two operations, and the
    /// archive is read out of it: two requests for one uncached source --
    /// the version check and someone browsing its files -- would otherwise
    /// clone or fetch into the same directory at once.
    checkout_locks: std::sync::Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>,
    checkout_root: PathBuf,
    aur_git_base_url: String,
    /// Where the `parse_network` setting is read from, when there is one.
    ///
    /// Parsing a PKGBUILD runs it, so it happens confined and without the
    /// network unless a deployment says otherwise; see
    /// [`crate::pkgbuild::Bridge`]. The setting is resolved per parse rather
    /// than once, so turning it on through the API applies to the next parse
    /// instead of the next restart. Tests construct a store without a database
    /// and get the default.
    db: Option<DatabaseConnection>,
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self::with_checkout_root(default_checkout_root())
    }

    pub fn with_checkout_root(checkout_root: PathBuf) -> Self {
        Self::with_checkout_root_and_aur_base(checkout_root, default_aur_git_base_url())
    }

    /// Construct a store with an explicit checkout root and AUR git base URL
    /// (e.g. `file:///tmp/fake-aur` in tests, instead of the real AUR).
    pub fn with_checkout_root_and_aur_base(
        checkout_root: PathBuf,
        aur_git_base_url: impl Into<String>,
    ) -> Self {
        Self {
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(CACHE_CAPACITY).expect("CACHE_CAPACITY must be non-zero"),
            )),
            checkout_locks: std::sync::Mutex::default(),
            checkout_root,
            aur_git_base_url: aur_git_base_url.into(),
            db: None,
        }
    }

    /// Attach the database the `parse_network` setting is stored in.
    #[must_use]
    pub fn with_db(mut self, db: DatabaseConnection) -> Self {
        self.db = Some(db);
        self
    }

    /// Whether a PKGBUILD parsed now may use the network.
    ///
    /// Global only: the packages that need it are identified by what their
    /// PKGBUILD does when sourced, which is known before any package row
    /// exists. A per-package override can layer on later by passing the id.
    async fn parse_network(&self) -> anyhow::Result<bool> {
        let Some(db) = &self.db else {
            return Ok(false);
        };
        Ok(
            crate::settings::get(db, crate::settings::key::PARSE_NETWORK, None)
                .await?
                .value,
        )
    }

    /// Return the parsed `.SRCINFO` for `source_data`, fetching it if not cached.
    ///
    /// `patch` is the raw JSON representation of a [`SourcePatch`] (as stored
    /// in `packages.patch`); pass `None` for unpatched sources. When a
    /// (non-empty) patch is given, `.SRCINFO` is always regenerated from the
    /// patched `PKGBUILD`.
    ///
    /// Returns an error if the (possibly patched) source's `.SRCINFO`/PKGBUILD
    /// could not be parsed - unlike [`SnapshotStore::list_files`]/
    /// [`SnapshotStore::read_file`], which work even for unparseable sources
    /// so a patch can be authored to fix the parse failure.
    pub async fn sourceinfo(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<SourceInfoV1> {
        self.get_or_fetch(source_data, patch).await?.active.parsed()
    }

    /// The `PKGBUILD` [`SnapshotStore::sourceinfo`] was parsed from: patched
    /// when `patch` is.
    pub async fn pkgbuild(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<String> {
        self.get_or_fetch(source_data, patch)
            .await?
            .active
            .pkgbuild()
    }

    /// Return the raw archive bytes for `source_data`, fetching it if not cached.
    ///
    /// See [`SnapshotStore::sourceinfo`] for the meaning of `patch`.
    pub async fn archive_bytes(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<Vec<u8>> {
        let entry = self.get_or_fetch(source_data, patch).await?;
        Ok(entry.active.archive_bytes.clone())
    }

    /// List the (unpatched) source files available for editing, relative to
    /// the source root (e.g. `PKGBUILD`, `foo.install`). Works even if the
    /// source's `.SRCINFO`/PKGBUILD fails to parse.
    pub async fn list_files(&self, source_data: &SourceData) -> anyhow::Result<Vec<String>> {
        let entry = self.get_or_fetch_any(source_data).await?;
        let original = entry.original();
        list_files_in_archive(&original.archive_bytes, &original.pkgbase)
    }

    /// Return the effective content of a single source file: the pristine
    /// content when `patch` is `None`, otherwise that content with the
    /// stored patch (if any) applied. Works even if the source's
    /// `.SRCINFO`/PKGBUILD fails to parse.
    pub async fn read_file(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
        rel_path: &str,
    ) -> anyhow::Result<String> {
        // Always read the pristine content, regardless of whatever patch (if
        // any) happens to already be cached for this source, since `patch`
        // here is applied fresh on top of it below.
        let entry = self.get_or_fetch_any(source_data).await?;
        let original_snapshot = entry.original();
        let original = read_file_from_archive(
            &original_snapshot.archive_bytes,
            &original_snapshot.pkgbase,
            rel_path,
        )?;

        match patch.map(SourcePatch::parse).transpose()? {
            None => Ok(original),
            Some(patch) => patch.apply_to_content(rel_path, &original),
        }
    }

    /// Like [`SnapshotStore::read_file`], but never fails solely because the
    /// stored patch no longer applies cleanly to the current pristine
    /// content. The pristine content is always there; for a file the patch
    /// touches, so is the stored diff, along with either the patched content
    /// or why it no longer applies. Intended for UI consumption, where the
    /// user should always be able to see (and revert to) the original content
    /// even when their patch is stale, and inspect the diff itself to
    /// understand what it does.
    pub async fn read_file_with_patch_status(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
        rel_path: &str,
    ) -> anyhow::Result<SourceFileContent> {
        let entry = self.get_or_fetch_any(source_data).await?;
        let original_snapshot = entry.original();
        let mut content = SourceFileContent {
            path: rel_path.to_string(),
            original_content: read_file_from_archive(
                &original_snapshot.archive_bytes,
                &original_snapshot.pkgbase,
                rel_path,
            )?,
            patched_content: None,
            patch_error: None,
            stored_patch: None,
        };

        let Some(patch) = patch.map(SourcePatch::parse).transpose()? else {
            return Ok(content);
        };
        let Some(diff) = patch.diff_for(rel_path) else {
            return Ok(content);
        };
        content.stored_patch = Some(diff.to_string());
        match patch.apply_to_content(rel_path, &content.original_content) {
            Ok(patched) => content.patched_content = Some(patched),
            Err(e) => content.patch_error = Some(e.to_string()),
        }
        Ok(content)
    }

    /// Metadata about a package read from its checkout: the `.SRCINFO`
    /// fields, the maintainer comment in the PKGBUILD, and the packaging
    /// history from git.
    ///
    /// Everything here is already on disk — this store cloned the repository
    /// to resolve the source in the first place — so it replaces what used to
    /// be a live AUR lookup per request.
    pub async fn source_metadata(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<crate::package::source_metadata::SourceMetadata> {
        let entry = self.get_or_fetch(source_data, patch).await?;
        Ok(self.metadata_from_entry(&entry, source_data).await)
    }

    /// The parsed `.SRCINFO`, the `PKGBUILD` and the metadata, from a single
    /// resolve.
    ///
    /// The version check needs all three: the `.SRCINFO` and `PKGBUILD` to
    /// sync VCS sources, and the metadata to mirror onto the row. Asking for
    /// them separately meant two `get_or_fetch` round trips per package per
    /// pass — cheap, since the second is served from cache, but pointless.
    pub async fn sourceinfo_and_metadata(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<Resolved> {
        let entry = self.get_or_fetch(source_data, patch).await?;
        Ok(Resolved {
            metadata: self.metadata_from_entry(&entry, source_data).await,
            sourceinfo: entry.active.parsed()?,
            pkgbuild: entry.active.pkgbuild()?,
        })
    }

    async fn metadata_from_entry(
        &self,
        entry: &CacheEntry,
        source_data: &SourceData,
    ) -> crate::package::source_metadata::SourceMetadata {
        use crate::package::source_metadata::{from_sourceinfo, maintainer_from_pkgbuild};

        let mut metadata = entry
            .active
            .sourceinfo
            .as_ref()
            .map(from_sourceinfo)
            .unwrap_or_default();

        // Best-effort: a PKGBUILD that cannot be read still leaves the
        // `.SRCINFO`-derived fields usable.
        if let Ok(pkgbuild) = entry.active.pkgbuild() {
            metadata.maintainer = maintainer_from_pkgbuild(&pkgbuild);
        }

        // Off the executor: walking the git history holds no `.await`.
        // Best-effort stays best-effort — a panicked walk still yields an
        // empty history, like an unreadable repository.
        let history_path = self
            .checkout_root
            .join(sanitize_cache_key(&source_data.cache_key()));
        let history = *entry
            .history
            .get_or_init(|| async move {
                tokio::task::spawn_blocking(move || Self::history_at(&history_path))
                    .await
                    .unwrap_or_default()
            })
            .await;
        metadata.first_submitted = history.first_submitted;
        metadata.last_modified = history.last_modified;
        metadata
    }

    /// When a package's packaging was first and last touched, from the commit
    /// history of its checkout.
    ///
    /// This is the packaging repository's history, not the upstream project's:
    /// for an AUR package it is exactly "first submitted" and "last modified".
    ///
    /// Returns an empty history rather than failing — a shallow or unreadable
    /// repository should cost two display fields, not the whole lookup.
    fn history_at(path: &std::path::Path) -> PackagingHistory {
        let Ok(repo) = git2::Repository::open(path) else {
            return PackagingHistory::default();
        };
        let Ok(mut walk) = repo.revwalk() else {
            return PackagingHistory::default();
        };
        if walk.push_head().is_err() {
            return PackagingHistory::default();
        }
        // The initial commit is at the end of the first-parent chain; merged
        // branches only add commits that are neither the first nor the last.
        walk.simplify_first_parent().ok();
        let time = |oid| repo.find_commit(oid).ok().map(|c| c.time().seconds());
        let newest = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(time);
        let oldest = walk.flatten().last().and_then(time);

        PackagingHistory {
            first_submitted: oldest,
            last_modified: newest,
        }
    }

    /// Forget a source: its cached snapshot and its persistent checkout.
    ///
    /// Called when a package is deleted, so the clone made for it does not
    /// outlive it. Absent directories are not an error -- a package that was
    /// never resolved has no checkout, and neither does one whose checkout a
    /// previous prune already took.
    ///
    /// `keep` is the sources of the packages that remain. A checkout one of
    /// them maps to is left alone, by the same rule as
    /// [`SnapshotStore::prune_orphaned_checkouts`]: cache keys are not unique
    /// per package (every upload shares one) and sanitising them is
    /// many-to-one, so the directory may still be another package's.
    pub async fn remove_checkout(
        &self,
        source_data: &SourceData,
        keep: &[SourceData],
    ) -> anyhow::Result<()> {
        let cache_key = source_data.cache_key();
        let dir_name = sanitize_cache_key(&cache_key);
        if keep
            .iter()
            .any(|live| sanitize_cache_key(&live.cache_key()) == dir_name)
        {
            return Ok(());
        }
        self.cache.lock().await.pop(&cache_key);

        let lock = self.checkout_lock(&dir_name);
        let _checkout = lock.lock().await;
        let path = self.checkout_root.join(dir_name);
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(anyhow::Error::new(e)
                .context(format!("could not remove checkout {}", path.display()))),
        }
    }

    /// Remove every checkout under the root that no live source claims.
    ///
    /// Deleting a package is not the only way to strand a checkout, which is
    /// why this exists alongside [`SnapshotStore::remove_checkout`] rather than
    /// as a backstop for it. An add resolves its whole dependency graph --
    /// cloning each package it plans -- before `persist_plan` writes a single
    /// row, so an add that fails part-way leaves clones behind that no row ever
    /// referred to and no delete path will ever visit.
    ///
    /// Directories are matched by sanitized cache key, the same name
    /// [`SnapshotStore`] checks out into. Sanitisation is many-to-one, so two
    /// sources can want the same directory; that can only make this keep a
    /// directory it might have removed, never remove one still in use.
    ///
    /// Best-effort per entry: a directory that cannot be removed is logged and
    /// skipped, since one unreadable checkout should not stop the rest. Returns
    /// how many were removed.
    ///
    /// **Call this only while nothing else is using the store.** It does not
    /// coordinate with in-flight fetches, so a clone that has begun but not yet
    /// been registered looks exactly like an orphan.
    pub async fn prune_orphaned_checkouts(&self, keep: &[SourceData]) -> anyhow::Result<usize> {
        let live: std::collections::HashSet<String> = keep
            .iter()
            .map(|source| sanitize_cache_key(&source.cache_key()))
            .collect();

        let mut entries = match tokio::fs::read_dir(&self.checkout_root).await {
            Ok(entries) => entries,
            // Nothing has been checked out yet, so nothing can be orphaned.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "could not read checkout root {}",
                    self.checkout_root.display()
                )));
            }
        };

        let mut removed = 0;
        while let Some(entry) = entries.next_entry().await? {
            // Only directories: a checkout is one, and anything else under the
            // root was put there by something that is not this store.
            if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            if name.to_str().is_some_and(|name| live.contains(name)) {
                continue;
            }
            let path = entry.path();
            match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => {
                    tracing::info!("removed orphaned source checkout {}", path.display());
                    removed += 1;
                }
                Err(e) => tracing::warn!(
                    "could not remove orphaned source checkout {}: {e}",
                    path.display()
                ),
            }
        }
        Ok(removed)
    }

    /// Proactively refresh the cache entry for `source_data`: fetch the
    /// latest state from the remote and, if the resolved ref actually moved
    /// (or there was no cached entry yet), re-parse/re-tar it.
    ///
    /// This is intended to be called from the periodic version-check loop so
    /// that staleness is detected (and long-lived caches kept honest) without
    /// unconditionally re-downloading/re-cloning on every check. If a patch
    /// was previously active for this source it is re-applied on top of the
    /// freshly fetched raw source, so the cache entry stays consistent.
    pub async fn refresh(&self, source_data: &SourceData) -> anyhow::Result<Refresh> {
        let cache_key = source_data.cache_key();
        let previous = {
            let mut cache = self.cache.lock().await;
            cache.get(&cache_key).cloned()
        };
        let previous_commit = previous.as_ref().map(|entry| entry.commit);

        // Read once (see the fetch path): both consumers below want the same
        // answer, and each call is a DB round trip.
        let network = self.parse_network().await?;
        let CheckedOutSource { commit, raw } = self.checkout(source_data, network).await?;

        let changed = previous_commit != Some(commit);
        if changed {
            // Re-apply whichever patch (if any) was previously active for
            // this source, so a `refresh` doesn't silently drop it.
            let existing_patch = previous.and_then(|entry| entry.patch.clone());
            // Off the executor, as above.
            let entry = tokio::task::spawn_blocking(move || {
                build_cache_entry(commit, raw, existing_patch, network)
            })
            .await??;
            self.cache.lock().await.put(cache_key, entry);
        }
        Ok(if changed {
            Refresh::Changed
        } else {
            Refresh::Current
        })
    }

    async fn get_or_fetch(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<Arc<CacheEntry>> {
        let cache_key = source_data.cache_key();
        let patch = match patch.map(SourcePatch::parse).transpose()? {
            Some(patch) if !patch.is_empty() => Some(patch),
            _ => None,
        };

        // Fast path: already cached with the exact same patch state.
        let cached = self.cache.lock().await.get(&cache_key).cloned();
        if let Some(entry) = &cached
            && Self::entry_matches_patch(entry, patch.as_ref())
        {
            return Ok(Arc::clone(entry));
        }

        // Read once: a DB round trip per call, and both consumers below want
        // the same answer.
        let network = self.parse_network().await?;
        // Cached under another patch: the pristine snapshot is already here,
        // and only the patch has to be applied again. Fetching is `refresh`'s
        // job; doing it here too cost a round trip to the remote and a fresh
        // parse every time a package's patch was edited or previewed.
        let CheckedOutSource { commit, raw } = match cached {
            Some(entry) => CheckedOutSource {
                commit: entry.commit,
                raw: entry.original().clone(),
            },
            None => self.checkout(source_data, network).await?,
        };

        // Off the executor: patching unpacks, re-tars and re-parses the whole
        // archive with no `.await` in between.
        let entry =
            tokio::task::spawn_blocking(move || build_cache_entry(commit, raw, patch, network))
                .await??;

        self.cache.lock().await.put(cache_key, Arc::clone(&entry));
        Ok(entry)
    }

    /// Fetch (or reuse) the raw checkout for `source_data`, regardless of
    /// whatever patch state (if any) happens to already be cached for it.
    /// Used by [`SnapshotStore::list_files`]/[`SnapshotStore::read_file`],
    /// which only ever need the pristine source (`entry.original()`) and
    /// must not force a redundant re-fetch just because a different/no
    /// patch is currently active in the cache.
    async fn get_or_fetch_any(&self, source_data: &SourceData) -> anyhow::Result<Arc<CacheEntry>> {
        let cache_key = source_data.cache_key();

        {
            let mut cache = self.cache.lock().await;
            if let Some(entry) = cache.get(&cache_key) {
                return Ok(Arc::clone(entry));
            }
        }

        self.get_or_fetch(source_data, None).await
    }

    /// Whether a cached entry already reflects the given patch state (both
    /// `None`, or both set to byte-identical patch content), so the fast
    /// path can be taken without redoing the (cheap but non-free) patch
    /// application step.
    fn entry_matches_patch(entry: &CacheEntry, patch: Option<&SourcePatch>) -> bool {
        entry.patch.as_ref() == patch
    }

    /// Bring `source_data`'s persistent checkout up to date and read it,
    /// holding that checkout's lock throughout.
    async fn checkout(
        &self,
        source_data: &SourceData,
        network: bool,
    ) -> anyhow::Result<CheckedOutSource> {
        let coordinates = self.git_coordinates(source_data)?;
        let dir_name = sanitize_cache_key(&source_data.cache_key());
        let lock = self.checkout_lock(&dir_name);
        let _checkout = lock.lock().await;
        checkout_and_parse(coordinates, &self.checkout_root.join(dir_name), network)
            .await
            .map_err(|e| explain_source_failure(source_data, e))
    }

    /// The lock of the checkout directory `dir_name`.
    fn checkout_lock(&self, dir_name: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .checkout_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(locks.entry(dir_name.to_string()).or_default())
    }

    /// Map a `SourceData` to the git coordinates used to fetch it.
    fn git_coordinates(&self, source_data: &SourceData) -> anyhow::Result<GitCoordinates> {
        match source_data {
            SourceData::Aur { name } => Ok(GitCoordinates {
                repo_url: format!("{}/{name}.git", self.aur_git_base_url),
                git_ref: "HEAD".to_string(),
                subfolder: String::new(),
            }),
            SourceData::Git { spec } => Ok(GitCoordinates {
                repo_url: spec.url.clone(),
                git_ref: spec.r#ref.clone(),
                subfolder: spec.subfolder.clone(),
            }),
            SourceData::Upload { .. } => anyhow::bail!("Upload sources are not yet supported"),
        }
    }
}

/// Say what an empty AUR repository actually means.
///
/// The AUR answers a clone for a package it does not have with an *empty*
/// repository rather than a 404, so a misspelled name gets all the way to the
/// checkout before anything notices. The error that comes back describes the
/// git-level symptom, which is a poor way to be told a package name is wrong --
/// and this is the most common way for an add to fail.
///
/// It cannot be caught earlier. The AUR RPC matches on *pkgname*, so a name
/// that resolves to nothing is ambiguous: it is what a nonexistent package
/// looks like, and equally what a real pkgbase with no child of the same name
/// looks like (`czkawka`, whose packages are `czkawka-cli` and `czkawka-gui`).
/// The empty clone is the first unambiguous evidence.
fn explain_source_failure(source_data: &SourceData, error: anyhow::Error) -> anyhow::Error {
    let SourceData::Aur { name } = source_data else {
        return error;
    };
    if error.downcast_ref::<EmptyRepository>().is_some() {
        return anyhow::anyhow!("no package named '{name}' in the AUR");
    }
    error
}

#[cfg(test)]
mod failure_tests {
    use super::{EmptyRepository, explain_source_failure};
    use aurcache_db::packages::SourceData;

    fn aur(name: &str) -> SourceData {
        SourceData::Aur {
            name: name.to_string(),
        }
    }

    /// The message a mistyped package name should produce. The AUR serves an
    /// empty repository rather than a 404, so without this the user is told
    /// their git ref is unresolvable.
    #[test]
    fn an_empty_aur_clone_says_the_package_does_not_exist() {
        let error = anyhow::Error::new(EmptyRepository {
            url: "https://aur.archlinux.org/nope.git".to_string(),
        });
        let explained = explain_source_failure(&aur("nope"), error).to_string();
        assert!(
            explained.contains("no package named 'nope' in the AUR"),
            "unhelpful message: {explained}"
        );
    }

    /// Only that failure is reinterpreted. An unparseable PKGBUILD in a package
    /// that does exist must keep saying so.
    #[test]
    fn other_failures_are_passed_through_untouched() {
        let error = anyhow::anyhow!("PKGBUILD parsing failed");
        let explained = explain_source_failure(&aur("real"), error).to_string();
        assert_eq!(explained, "PKGBUILD parsing failed");
    }

    /// A user-supplied git remote that is empty is empty -- there is no AUR to
    /// blame, and saying "not in the AUR" would be wrong.
    #[test]
    fn a_git_source_keeps_the_git_level_reason() {
        let error = anyhow::Error::new(EmptyRepository {
            url: "https://example.com/x.git".to_string(),
        });
        let source = SourceData::Git {
            spec: aurcache_common::source::GitSourceSpec {
                url: "https://example.com/x.git".to_string(),
                r#ref: "main".to_string(),
                subfolder: String::new(),
            },
        };
        let explained = explain_source_failure(&source, error).to_string();
        assert!(
            explained.contains("is empty"),
            "lost the reason: {explained}"
        );
        assert!(!explained.contains("AUR"));
    }
}

/// Turn a `cache_key()` into a filesystem-safe directory name.
fn sanitize_cache_key(cache_key: &str) -> String {
    cache_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Unix timestamps of the first and latest commit of a packaging checkout.
#[derive(Clone, Copy, Default)]
struct PackagingHistory {
    first_submitted: Option<i64>,
    last_modified: Option<i64>,
}

/// Where a source's PKGBUILD lives in git.
struct GitCoordinates {
    repo_url: String,
    git_ref: String,
    /// Directory within the repository holding the PKGBUILD/.SRCINFO; empty
    /// for the repository root.
    subfolder: String,
}

/// A source as [`checkout_and_parse`] found it, before any patch.
struct CheckedOutSource {
    commit: Oid,
    raw: SourceSnapshot,
}

/// Checkout (or fetch-and-update) `repo_url`@`git_ref` into the persistent
/// `path`, then parse the `.SRCINFO`/PKGBUILD and build a tar.gz archive of
/// `subfolder` (or the repo root if empty) with `{pkgbase}/` as the
/// top-level directory, matching the structure of AUR snapshots.
///
/// A shipped `.SRCINFO` that does not parse is not the end of it: the file is
/// only derived from the PKGBUILD, and maintainers do edit it by hand --
/// `ogdf`'s carries a `pkgtreename=foxglove` line `makepkg --printsrcinfo`
/// would never write, which failed every package depending on it. So the
/// PKGBUILD is parsed instead, as it is for a source that ships no `.SRCINFO`.
///
/// If that fails too, `sourceinfo` is `None` rather than the whole fetch
/// failing, so the raw source can still be browsed/edited (and a patch
/// authored to fix the parse failure) before it's ever successfully added as
/// a package. A best-effort `pkgbase` is derived from the repo URL/subfolder
/// in that case.
async fn checkout_and_parse(
    coordinates: GitCoordinates,
    path: &Path,
    network: bool,
) -> anyhow::Result<CheckedOutSource> {
    use crate::git::checkout::checkout_or_fetch_repo_ref;
    use crate::pkgbuild::parse_pkgbuild;

    let GitCoordinates {
        repo_url,
        git_ref,
        subfolder,
    } = coordinates;
    let path = path.to_path_buf();

    // git2 types are not `Send`, so the checkout itself must run fully
    // within the blocking closure.
    let (commit, package_dir) = tokio::task::spawn_blocking(move || {
        let commit = checkout_or_fetch_repo_ref(&repo_url, &git_ref, &path)?;
        let package_dir = if subfolder.is_empty() {
            path
        } else {
            let package_dir = path.join(&subfolder);
            ensure_inside(&package_dir, &path)?;
            package_dir
        };
        anyhow::Ok((commit, package_dir))
    })
    .await??;

    // Parsing and archiving run in the same blocking closure as the checkout
    // above: PKGBUILD parsing shells out to a bridge script and archiving
    // tar+gzs the whole tree, and neither holds an `.await` — running them on
    // the executor would stall unrelated tasks.
    let raw = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let srcinfo_path = package_dir.join(".SRCINFO");
        let pkgbuild_path = package_dir.join("PKGBUILD");
        let parsed = if srcinfo_path.exists() {
            read_checkout_file(&srcinfo_path)
                .map_err(anyhow::Error::from)
                .and_then(|content| Ok(SourceInfoV1::from_string(&fix_source_urls(&content))?))
                .or_else(|err| {
                    tracing::warn!(
                        "{} does not parse, parsing the PKGBUILD instead: {err:#}",
                        srcinfo_path.display()
                    );
                    parse_pkgbuild(&pkgbuild_path, network)
                })
        } else {
            parse_pkgbuild(&pkgbuild_path, network)
        };

        let (pkgbase, sourceinfo) = match parsed {
            Ok(sourceinfo) => (sourceinfo.base.name.to_string(), Some(sourceinfo)),
            Err(err) => {
                tracing::warn!("{} could not be parsed: {err:#}", pkgbuild_path.display());
                (fallback_pkgbase(&package_dir), None)
            }
        };

        let archive_bytes = create_archive_with_pkgbase_dir(&package_dir, &pkgbase)?;

        Ok(SourceSnapshot {
            archive_bytes,
            sourceinfo,
            pkgbase,
        })
    })
    .await??;

    Ok(CheckedOutSource { commit, raw })
}

/// Read a file from a package checkout, which the package's repository wrote.
///
/// Only a regular file is read. The server reads these unconfined, and a
/// repository may commit a symlink -- `.SRCINFO` pointing at the server's
/// environment file would otherwise be read, parsed and its lines reported.
/// A symlinked file is treated as unreadable, which sends a `.SRCINFO` to the
/// sandboxed PKGBUILD parse instead.
fn read_checkout_file(path: &Path) -> std::io::Result<String> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }
    std::fs::read_to_string(path)
}

/// Refuse a package directory that resolves outside its checkout: a
/// repository can make the configured subfolder a symlink to anywhere, and
/// everything under the package directory is archived and served to workers.
fn ensure_inside(package_dir: &Path, checkout: &Path) -> anyhow::Result<()> {
    let resolved = package_dir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("resolving {}: {e}", package_dir.display()))?;
    let root = checkout
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("resolving {}: {e}", checkout.display()))?;
    if !resolved.starts_with(&root) {
        anyhow::bail!(
            "the package directory {} resolves outside its checkout, to {}",
            package_dir.display(),
            resolved.display()
        );
    }
    Ok(())
}

/// Best-effort pkgbase name to use as the archive's top-level directory when
/// `.SRCINFO`/PKGBUILD parsing fails: read `pkgbase=`/`pkgname=` directly out
/// of the PKGBUILD text, falling back to the checkout directory's name.
fn fallback_pkgbase(package_dir: &Path) -> String {
    if let Ok(content) = read_checkout_file(&package_dir.join("PKGBUILD")) {
        for line in content.lines() {
            let line = line.trim();
            for prefix in ["pkgbase=", "pkgname="] {
                if let Some(value) = line.strip_prefix(prefix) {
                    let value = value.trim_matches(['"', '\''].as_ref()).trim();
                    if !value.is_empty() {
                        return value.to_string();
                    }
                }
            }
        }
    }
    package_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "source".to_string())
}

/// Apply `patch` on top of an already-fetched (unpatched) snapshot,
/// regenerating `.SRCINFO` from the patched `PKGBUILD` and re-packaging the
/// result into a new tar.gz with the same `{pkgbase}/` layout.
///
/// This operates entirely in memory: the archive is unpacked into a
/// `{relative path -> bytes}` map, the patch is applied to that map, and the
/// result is re-tarred directly from memory. The lone exception is the
/// (patched) `PKGBUILD`, which is written to a short-lived temp file purely
/// because parsing it shells out to a bash script (`alpm-pkgbuild-bridge`)
/// that needs a real path to `source` - the file is removed again as soon as
/// parsing finishes.
fn apply_patch_to_archive(
    raw: &SourceSnapshot,
    patch: &SourcePatch,
    network: bool,
) -> anyhow::Result<SourceSnapshot> {
    use crate::pkgbuild::parse_pkgbuild_content;

    let mut archive = extract_tar_gz_to_memory(&raw.archive_bytes)?;

    for rel_path in patch.paths() {
        // A file keeps its mode; one the patch creates is an ordinary 0644.
        let (mode, original) = match archive.entries.get(rel_path) {
            None => (0o644, Vec::new()),
            Some(MemoryEntry {
                mode,
                kind: EntryKind::File(content),
            }) => (*mode, content.clone()),
            Some(_) => anyhow::bail!("'{rel_path}' is a symlink or a directory, cannot patch it"),
        };
        let original = String::from_utf8(original)
            .map_err(|_| anyhow::anyhow!("File '{rel_path}' is not valid UTF-8, cannot patch"))?;
        let patched = patch.apply_to_content(rel_path, &original)?;
        archive.entries.insert(
            rel_path.to_string(),
            MemoryEntry {
                mode,
                kind: EntryKind::File(patched.into_bytes()),
            },
        );
    }

    let pkgbuild = match archive.entries.get("PKGBUILD") {
        Some(MemoryEntry {
            kind: EntryKind::File(content),
            ..
        }) => content,
        _ => anyhow::bail!("Archive has no PKGBUILD to parse"),
    };
    let pkgbuild = std::str::from_utf8(pkgbuild)
        .map_err(|_| anyhow::anyhow!("PKGBUILD is not valid UTF-8, cannot parse"))?;
    let sourceinfo = parse_pkgbuild_content(pkgbuild, network)?;

    Ok(SourceSnapshot {
        archive_bytes: create_archive_from_memory(&archive)?,
        sourceinfo: Some(sourceinfo),
        pkgbase: raw.pkgbase.clone(),
    })
}

/// Build a [`CacheEntry`] from a fresh raw snapshot, applying `patch` on top
/// if one is active. Used by both the hot path ([`SnapshotStore::get_or_fetch`])
/// and the refresh path so the patched/no-patch shape stays in one place.
fn build_cache_entry(
    commit: Oid,
    raw: SourceSnapshot,
    patch: Option<SourcePatch>,
    network: bool,
) -> anyhow::Result<Arc<CacheEntry>> {
    Ok(match patch {
        None => Arc::new(CacheEntry {
            commit,
            active: raw,
            original: None,
            patch: None,
            history: tokio::sync::OnceCell::new(),
        }),
        Some(patch) => {
            let patched = apply_patch_to_archive(&raw, &patch, network)?;
            Arc::new(CacheEntry {
                commit,
                active: patched,
                original: Some(raw),
                patch: Some(patch),
                history: tokio::sync::OnceCell::new(),
            })
        }
    })
}

/// A `{pkgbase}/...` source archive, held in memory to be patched.
struct MemoryArchive {
    /// The top-level directory every entry sits in.
    pkgbase: String,
    /// Every entry, by its path relative to that directory. Sorted, so a
    /// directory comes before what it holds when the archive is written back.
    entries: BTreeMap<String, MemoryEntry>,
}

/// One entry of a [`MemoryArchive`], with its permission bits.
struct MemoryEntry {
    mode: u32,
    kind: EntryKind,
}

enum EntryKind {
    File(Vec<u8>),
    /// A symlink, and where it points: kept as a link, never followed.
    Symlink(PathBuf),
    Dir,
}

/// Unpack a `{pkgbase}/...` tar.gz archive entirely into memory.
///
/// Files, directories and symlinks are all kept, each with its mode: a
/// patched source must be the unpatched one with only the patched files'
/// contents changed. Keeping regular files alone dropped every symlink and
/// empty directory, and writing them back as 0644 took the executable bit off
/// every script a PKGBUILD runs from its source directory.
fn extract_tar_gz_to_memory(archive_bytes: &[u8]) -> anyhow::Result<MemoryArchive> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);

    let mut pkgbase = None;
    let mut entries = BTreeMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        let Some((dir, rel_path)) = path.split_once('/') else {
            continue;
        };
        let rel_path = rel_path.trim_end_matches('/');
        if rel_path.is_empty() {
            continue;
        }
        if pkgbase.is_none() {
            pkgbase = Some(dir.to_string());
        }
        let mode = entry.header().mode()?;
        let kind = match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                EntryKind::File(buf)
            }
            tar::EntryType::Symlink => {
                let Some(target) = entry.link_name()? else {
                    continue;
                };
                EntryKind::Symlink(target.into_owned())
            }
            tar::EntryType::Directory => EntryKind::Dir,
            // Nothing a package checkout holds; the archiver makes none.
            _ => continue,
        };
        entries.insert(rel_path.to_string(), MemoryEntry { mode, kind });
    }

    let pkgbase = pkgbase
        .ok_or_else(|| anyhow::anyhow!("Extracted archive did not contain a pkgbase directory"))?;
    Ok(MemoryArchive { pkgbase, entries })
}

/// Re-package a [`MemoryArchive`] as a `{pkgbase}/...` tar.gz, entries as they
/// were read: files with their modes, symlinks as links, directories.
fn create_archive_from_memory(archive: &MemoryArchive) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    for (rel_path, entry) in &archive.entries {
        let path = format!("{}/{rel_path}", archive.pkgbase);
        let mut header = tar::Header::new_gnu();
        header.set_mode(entry.mode);
        match &entry.kind {
            EntryKind::File(content) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(content.len() as u64);
                header.set_cksum();
                tar.append_data(&mut header, path, content.as_slice())?;
            }
            EntryKind::Symlink(target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                tar.append_link(&mut header, path, target)?;
            }
            EntryKind::Dir => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                tar.append_data(&mut header, path, std::io::empty())?;
            }
        }
    }
    // Finish explicitly: dropping the encoder would swallow a compression error.
    tar.into_inner()?.finish()?;
    Ok(buf)
}

/// List files (relative to the source root) available for viewing/editing.
///
/// Excludes `.SRCINFO`, which is always regenerated and so is never a
/// meaningful patch target. Git metadata never reaches the archive in the
/// first place (see [`create_archive_with_pkgbase_dir`]).
fn list_files_in_archive(archive_bytes: &[u8], pkgbase: &str) -> anyhow::Result<Vec<String>> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);

    let prefix = format!("{pkgbase}/");
    let mut files = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().to_string();
        let Some(rel_path) = path.strip_prefix(&prefix) else {
            continue;
        };
        if rel_path == ".SRCINFO" {
            continue;
        }
        files.push(rel_path.to_string());
    }
    files.sort_unstable();
    Ok(files)
}

/// Read a single file's content out of a `{pkgbase}/...` tar.gz archive.
fn read_file_from_archive(
    archive_bytes: &[u8],
    pkgbase: &str,
    rel_path: &str,
) -> anyhow::Result<String> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);

    let target = format!("{pkgbase}/{rel_path}");
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        if path == target {
            let mut buf = String::new();
            entry.read_to_string(&mut buf)?;
            return Ok(buf);
        }
    }
    anyhow::bail!("File '{rel_path}' not found in source")
}

/// Package `source_dir` as a tar.gz whose single top-level directory is
/// `pkgbase`, matching the layout of an AUR snapshot.
///
/// `source_dir` is the persistent git checkout, so its git metadata sits right
/// next to the sources. `.git` is excluded: the archive is a source snapshot
/// served to workers, and shipping it would attach the repository's entire
/// history to every job download. Only the top level is filtered, which is
/// sufficient because the checkout is a plain clone with no submodules.
///
/// Symlinks are archived as symlinks. `tar::Builder` follows them by default,
/// and this runs in the server, unconfined, over files a package's repository
/// wrote: a committed `x -> /proc/self/environ` or `-> /etc/aurcache/...` would
/// put the target's contents -- the server's secrets -- into an archive every
/// worker, and the build that package runs there, can read. A symlink is what
/// makepkg expects anyway.
fn create_archive_with_pkgbase_dir(source_dir: &Path, pkgbase: &str) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.follow_symlinks(false);

    let root = Path::new(pkgbase);
    tar.append_dir(root, source_dir)?;
    for entry in std::fs::read_dir(source_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == GIT_METADATA_DIR {
            continue;
        }
        let dest = root.join(&name);
        if entry.file_type()?.is_dir() {
            tar.append_dir_all(dest, entry.path())?;
        } else {
            tar.append_path_with_name(entry.path(), dest)?;
        }
    }

    // Finish explicitly: dropping the encoder would swallow a compression error.
    tar.into_inner()?.finish()?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::SourcePatch;
    use git2::{Repository, Signature};

    const PKGBUILD: &str = "\
pkgname=foo
pkgbase=foo
pkgver=1.0
pkgrel=1
pkgdesc=\"A test package\"
url=\"https://example.org/\"
arch=('x86_64')
license=('MIT')
";

    /// One entry of an archive, as a test inspects it.
    struct ArchivedEntry {
        path: String,
        kind: tar::EntryType,
        mode: u32,
        /// Where a link points, for a symlink or a hard link.
        link: Option<String>,
        data: Vec<u8>,
    }

    /// Every entry of a tar.gz.
    fn archive_entries(bytes: &[u8]) -> Vec<ArchivedEntry> {
        use std::io::Read;
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().to_string_lossy().to_string();
                let kind = entry.header().entry_type();
                let mode = entry.header().mode().unwrap();
                let link = entry
                    .link_name()
                    .unwrap()
                    .map(|l| l.to_string_lossy().to_string());
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                ArchivedEntry {
                    path,
                    kind,
                    mode,
                    link,
                    data,
                }
            })
            .collect()
    }

    /// A repository can commit a symlink to anything. The archive is built by
    /// the server, unconfined, and served to every worker: a followed link
    /// would carry the target's contents -- the server's secrets -- to the
    /// build. It must stay a link, and a linked directory must not be walked.
    #[test]
    fn a_committed_symlink_is_archived_as_a_link_not_its_target() {
        let secrets = tempfile::tempdir().unwrap();
        let secret = secrets.path().join("server.env");
        std::fs::write(&secret, "DB_PWD=hunter2\n").unwrap();
        std::fs::create_dir(secrets.path().join("ca")).unwrap();
        std::fs::write(secrets.path().join("ca/key.pem"), "PRIVATE KEY").unwrap();

        let checkout = tempfile::tempdir().unwrap();
        let dir = checkout.path();
        std::fs::write(dir.join("PKGBUILD"), "pkgname=demo\n").unwrap();
        std::os::unix::fs::symlink(&secret, dir.join("leak")).unwrap();
        std::os::unix::fs::symlink(secrets.path().join("ca"), dir.join("leakdir")).unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::os::unix::fs::symlink(&secret, dir.join("sub/deeper")).unwrap();

        let entries = archive_entries(&create_archive_with_pkgbase_dir(dir, "demo").unwrap());
        for entry in &entries {
            let text = String::from_utf8_lossy(&entry.data);
            assert!(
                !text.contains("hunter2") && !text.contains("PRIVATE KEY"),
                "{} carries a secret",
                entry.path
            );
        }
        let find = |p: &str| {
            entries
                .iter()
                .find(|e| e.path == p)
                .unwrap_or_else(|| panic!("{p}"))
        };
        for (path, target) in [
            ("demo/leak", &secret),
            ("demo/sub/deeper", &secret),
            ("demo/leakdir", &secrets.path().join("ca")),
        ] {
            let entry = find(path);
            assert_eq!(entry.kind, tar::EntryType::Symlink, "{path}");
            assert_eq!(
                entry.link.as_deref(),
                Some(target.to_str().unwrap()),
                "{path}"
            );
        }
        assert!(
            !entries.iter().any(|e| e.path.starts_with("demo/leakdir/")),
            "a linked directory was walked"
        );
    }

    /// Patching rewrites the archive from memory, and a patched source must
    /// be the original with only the patched files changed: scripts keep
    /// their executable bit, links stay links, empty directories stay.
    #[test]
    fn an_archive_rebuilt_from_memory_keeps_modes_links_and_directories() {
        use std::os::unix::fs::PermissionsExt;
        let checkout = tempfile::tempdir().unwrap();
        let dir = checkout.path();
        std::fs::write(dir.join("PKGBUILD"), "pkgname=demo\n").unwrap();
        std::fs::write(dir.join("build.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(dir.join("build.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("PKGBUILD", dir.join("link")).unwrap();
        std::fs::create_dir(dir.join("empty")).unwrap();
        std::fs::set_permissions(dir.join("empty"), std::fs::Permissions::from_mode(0o700))
            .unwrap();

        let original = create_archive_with_pkgbase_dir(dir, "demo").unwrap();
        let memory = extract_tar_gz_to_memory(&original).unwrap();
        assert_eq!(memory.pkgbase, "demo");
        let rebuilt = archive_entries(&create_archive_from_memory(&memory).unwrap());
        let find = |p: &str| {
            rebuilt
                .iter()
                .find(|e| e.path == p)
                .unwrap_or_else(|| panic!("{p} was dropped"))
        };

        let script = find("demo/build.sh");
        assert_eq!(script.kind, tar::EntryType::Regular);
        assert_eq!(script.mode & 0o777, 0o755, "the executable bit");
        assert_eq!(script.data, b"#!/bin/sh\n");
        let link = find("demo/link");
        assert_eq!(link.kind, tar::EntryType::Symlink);
        assert_eq!(link.link.as_deref(), Some("PKGBUILD"));
        let empty = find("demo/empty");
        assert_eq!(empty.kind, tar::EntryType::Directory);
        assert_eq!(empty.mode & 0o777, 0o700);
    }

    /// `.SRCINFO` and the pkgbase fallback read the checkout unconfined, so a
    /// symlinked one is not read at all.
    #[test]
    fn a_symlinked_checkout_file_is_not_read() {
        let secrets = tempfile::tempdir().unwrap();
        let secret = secrets.path().join("server.env");
        std::fs::write(&secret, "pkgname=hunter2\n").unwrap();
        let checkout = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&secret, checkout.path().join(".SRCINFO")).unwrap();
        std::os::unix::fs::symlink(&secret, checkout.path().join("PKGBUILD")).unwrap();

        assert!(read_checkout_file(&checkout.path().join(".SRCINFO")).is_err());
        assert_ne!(
            fallback_pkgbase(checkout.path()),
            "hunter2",
            "the pkgbase came out of the linked file"
        );
        std::fs::write(checkout.path().join("real"), "ok").unwrap();
        assert_eq!(
            read_checkout_file(&checkout.path().join("real")).unwrap(),
            "ok"
        );
    }

    /// The package's subfolder is configured on the server, but whether it is
    /// a directory or a symlink out of the checkout is the repository's say.
    #[test]
    fn a_subfolder_that_resolves_outside_the_checkout_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        std::fs::create_dir(checkout.path().join("pkg")).unwrap();
        std::os::unix::fs::symlink(outside.path(), checkout.path().join("escape")).unwrap();

        assert!(ensure_inside(&checkout.path().join("pkg"), checkout.path()).is_ok());
        let error = ensure_inside(&checkout.path().join("escape"), checkout.path()).unwrap_err();
        assert!(
            format!("{error:#}").contains("outside its checkout"),
            "{error:#}"
        );
    }

    fn make_fixture_archive(pkgbase: &str, pkgbuild: &str) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let package_dir = dir.path().join(pkgbase);
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(package_dir.join("PKGBUILD"), pkgbuild).unwrap();
        // Include a stale/mismatched .SRCINFO to prove it's ignored once a
        // patch is applied - the regenerated one must come from PKGBUILD.
        std::fs::write(
            package_dir.join(".SRCINFO"),
            "pkgbase = foo\n\tpkgver = 0.0.1\n\tpkgrel = 1\n\narch = x86_64\n\npkgname = foo\n",
        )
        .unwrap();
        create_archive_with_pkgbase_dir(&package_dir, pkgbase).unwrap()
    }

    /// `parse_pkgbuild` runs an external bridge script through
    /// `aurcache-sandbox`; see [`crate::pkgbuild::tests::bridge_available`].
    ///
    /// Anything that applies a `SourcePatch` needs it too: patching regenerates
    /// `.SRCINFO` from the patched `PKGBUILD` via the same bridge.
    fn pkgbuild_bridge_available() -> bool {
        crate::pkgbuild::tests::bridge_available()
    }

    /// Every entry path in an archive, directories included.
    ///
    /// Deliberately raw rather than going through [`list_files_in_archive`],
    /// which filters for the UI: the point is to assert on what the tarball
    /// actually carries.
    fn archive_entry_paths(archive_bytes: &[u8]) -> Vec<String> {
        let decoder = flate2::read::GzDecoder::new(archive_bytes);
        let mut archive = tar::Archive::new(decoder);
        archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect()
    }

    fn parse_srcinfo_from_archive(archive_bytes: &[u8], pkgbase: &str) -> SourceInfoV1 {
        let content = read_file_from_archive(archive_bytes, pkgbase, ".SRCINFO").unwrap();
        SourceInfoV1::from_string(&fix_source_urls(&content)).unwrap()
    }

    /// A versioned dependency on a library name, as paru's `.SRCINFO` has.
    ///
    /// alpm-types 0.12 (with alpm-srcinfo 0.6.4) reads `libalpm.so>=14` as a
    /// malformed soname and rejects the whole file, so paru -- among the AUR's
    /// most popular packages -- could not be added or updated. The alpm crates
    /// are held at the release before for that; this is what tells whether a
    /// newer one can be taken.
    #[test]
    fn a_versioned_dependency_on_a_library_name_parses() {
        let srcinfo = "pkgbase = paru\n\
                       \tpkgver = 2.1.0\n\
                       \tpkgrel = 2\n\
                       \tarch = x86_64\n\
                       \tdepends = pacman\n\
                       \tdepends = libalpm.so>=14\n\
                       \n\
                       pkgname = paru\n";
        let parsed = SourceInfoV1::from_string(srcinfo);
        assert!(parsed.is_ok(), "{:?}", parsed.err());
    }

    /// A provision whose name continues past `.so`, as librtmp0's does.
    ///
    /// The same alpm-types 0.12 fault as above: `librtmp.so` reads as a soname
    /// and the `.0` after it is left over, so the package name it actually is
    /// never gets tried.
    #[test]
    fn a_provision_named_after_a_versioned_library_parses() {
        let srcinfo = "pkgbase = librtmp0\n\
                       \tpkgver = 2.6\n\
                       \tpkgrel = 1\n\
                       \tarch = x86_64\n\
                       \tdepends = gnutls\n\
                       \tprovides = librtmp.so.0\n\
                       \n\
                       pkgname = librtmp0\n";
        let parsed = SourceInfoV1::from_string(srcinfo);
        assert!(parsed.is_ok(), "{:?}", parsed.err());
    }

    #[test]
    fn apply_patch_to_archive_regenerates_srcinfo_from_patched_pkgbuild() {
        if !pkgbuild_bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }

        let raw = SourceSnapshot {
            archive_bytes: make_fixture_archive("foo", PKGBUILD),
            sourceinfo: None,
            pkgbase: "foo".to_string(),
        };

        let mut patch = SourcePatch::default();
        let patched_pkgbuild = PKGBUILD.replace("pkgver=1.0", "pkgver=2.0");
        patch.merge_file("PKGBUILD", PKGBUILD, &patched_pkgbuild);

        let patched = apply_patch_to_archive(&raw, &patch, false).unwrap();
        let new_archive_bytes = patched.archive_bytes;

        // .SRCINFO was regenerated from the patched PKGBUILD (version 2.0),
        // not copied over from the (stale, unpatched) shipped .SRCINFO.
        assert_eq!(
            patched.sourceinfo.unwrap().base.version.to_string(),
            "2.0-1"
        );

        let files = list_files_in_archive(&new_archive_bytes, "foo").unwrap();
        assert!(files.contains(&"PKGBUILD".to_string()));
        // The shipped .SRCINFO must never be surfaced as an editable/patchable file.
        assert!(!files.contains(&".SRCINFO".to_string()));

        let pkgbuild_content =
            read_file_from_archive(&new_archive_bytes, "foo", "PKGBUILD").unwrap();
        assert_eq!(pkgbuild_content, patched_pkgbuild);
    }

    #[test]
    fn snapshot_store_falls_back_to_shipped_srcinfo_without_patch() {
        // Without a patch, list_files/read_file/sourceinfo & archive_bytes on
        // an AUR-shaped source all operate on the raw fetched archive (i.e.
        // the shipped, possibly stale, .SRCINFO is what's used).
        let archive_bytes = make_fixture_archive("foo", PKGBUILD);
        let sourceinfo = parse_srcinfo_from_archive(&archive_bytes, "foo");
        assert_eq!(sourceinfo.base.version.to_string(), "0.0.1-1");
    }

    /// Create a local git repository at `aur_root/{pkgbase}.git` containing
    /// a PKGBUILD + a matching `.SRCINFO`, standing in for the real AUR git
    /// remote in tests.
    fn create_aur_git_repo(aur_root: &Path, pkgbase: &str, version: &str) -> PathBuf {
        let repo_path = aur_root.join(format!("{pkgbase}.git"));

        let pkgbuild = format!(
            "pkgname={pkgbase}\npkgver={version}\npkgrel=1\narch=('x86_64')\ndepends=()\nsource=()\nsha256sums=()\npackage() {{\n  :\n}}\n"
        );
        let srcinfo = format!(
            "pkgbase = {pkgbase}\n\tpkgver = {version}\n\tpkgrel = 1\n\narch = x86_64\n\npkgname = {pkgbase}\n"
        );

        crate::test_git::create_repo(
            &repo_path,
            &[("PKGBUILD", &pkgbuild), (".SRCINFO", &srcinfo)],
        );
        repo_path
    }

    /// A hand-edited `.SRCINFO` that `alpm-srcinfo` rejects must not make the
    /// source unparseable while its PKGBUILD is fine: `ogdf` shipped exactly
    /// this line, and every package depending on it failed to add.
    #[tokio::test]
    async fn unparseable_srcinfo_falls_back_to_pkgbuild() {
        if !pkgbuild_bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }

        let aur_root = tempfile::tempdir().unwrap();
        let repo_path = create_aur_git_repo(aur_root.path(), "bar", "1.0");
        let repo = Repository::open(&repo_path).unwrap();
        std::fs::write(
            repo_path.join(".SRCINFO"),
            "pkgbase = bar\n\tpkgtreename=foxglove\n\tpkgver = 1.0\n\tpkgrel = 1\n\tarch = x86_64\n\npkgname = bar\n",
        )
        .unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(".SRCINFO")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "hand-edit .SRCINFO",
            &tree,
            &[&parent],
        )
        .unwrap();

        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let source = SourceData::Aur {
            name: "bar".to_string(),
        };

        let sourceinfo = store.sourceinfo(&source, None).await.unwrap();
        assert_eq!(sourceinfo.base.name.to_string(), "bar");
        assert_eq!(sourceinfo.base.version.to_string(), "1.0-1");
    }

    /// A store, and the directory its checkouts live in, which goes when this
    /// does.
    struct TestStore {
        store: SnapshotStore,
        _checkouts: tempfile::TempDir,
    }

    /// Build a `SnapshotStore` for tests: AUR sources resolve against local
    /// git repos under `aur_root` instead of the real AUR.
    fn test_store(aur_root: &Path) -> TestStore {
        let checkout_dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root_and_aur_base(
            checkout_dir.path().to_path_buf(),
            aur_root.to_string_lossy().to_string(),
        );
        TestStore {
            store,
            _checkouts: checkout_dir,
        }
    }

    fn some_patch(new_pkgver: &str) -> SourcePatch {
        let original =
            "pkgname=bar\npkgver=1.0\npkgrel=1\narch=('x86_64')\ndepends=()\nsource=()\nsha256sums=()\npackage() {\n  :\n}\n"
                .to_string();
        let patched = original.replace("pkgver=1.0", &format!("pkgver={new_pkgver}"));
        let mut patch = SourcePatch::default();
        patch.merge_file("PKGBUILD", &original, &patched);
        patch
    }

    /// First submitted is the initial commit, last modified is HEAD -- with a
    /// merged branch in between, whose commits are neither.
    #[test]
    fn history_is_the_initial_commit_and_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let tree = {
            let mut index = repo.index().unwrap();
            repo.find_tree(index.write_tree().unwrap()).unwrap()
        };
        let commit = |at: i64, parents: &[&git2::Commit<'_>], update: Option<&str>| {
            let sig = Signature::new("Test", "test@example.com", &git2::Time::new(at, 0)).unwrap();
            let oid = repo
                .commit(update, &sig, &sig, "c", &tree, parents)
                .unwrap();
            repo.find_commit(oid).unwrap()
        };
        let root = commit(1_000, &[], None);
        // Older than the root by its clock: a side branch is not ordered.
        let side = commit(500, &[&root], None);
        let main = commit(2_000, &[&root], None);
        commit(3_000, &[&main, &side], Some("HEAD"));

        let history = SnapshotStore::history_at(tmp.path());
        assert_eq!(history.first_submitted, Some(1_000));
        assert_eq!(history.last_modified, Some(3_000));
    }

    /// Commit a PKGBUILD (with a fixed pkgbase of `bar`, version `1.0`) plus
    /// a matching `.SRCINFO` to `main` in `repo`, creating the branch on the
    /// first call and extending its history thereafter. `marker` is added as
    /// a harmless trailing comment so each commit's tree differs.
    fn commit_to_repo(repo: &Repository, marker: &str, message: &str) {
        let pkgbuild = format!(
            "pkgname=bar\npkgver=1.0\npkgrel=1\narch=('x86_64')\ndepends=()\nsource=()\nsha256sums=()\npackage() {{\n  :\n}}\n# {marker}\n"
        );
        let srcinfo =
            "pkgbase = bar\n\tpkgver = 1.0\n\tpkgrel = 1\n\narch = x86_64\n\npkgname = bar\n";

        std::fs::write(repo.workdir().unwrap().join("PKGBUILD"), pkgbuild).unwrap();
        std::fs::write(repo.workdir().unwrap().join(".SRCINFO"), srcinfo).unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.add_path(Path::new(".SRCINFO")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        let parents = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| repo.find_commit(oid).ok())
            .map(|commit| vec![commit])
            .unwrap_or_default();
        let parent_refs = parents.iter().collect::<Vec<_>>();
        repo.commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            message,
            &tree,
            &parent_refs,
        )
        .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(None).unwrap();
    }

    /// The source archive served to workers must not carry the checkout's git
    /// metadata: `.git` holds the repository's entire history, and the archive
    /// is downloaded fresh for every build job.
    ///
    /// This drives the real path — resolve source, clone into the persistent
    /// checkout, build the archive — so a regression anywhere along it fails
    /// here rather than only showing up as bloated job downloads.
    #[tokio::test]
    async fn fetched_archive_excludes_git_metadata() {
        let aur_root = tempfile::tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "bar", "1.0");
        let TestStore {
            store,
            _checkouts: checkout_dir,
        } = test_store(aur_root.path());
        let source = SourceData::Aur {
            name: "bar".to_string(),
        };

        let archive = store.archive_bytes(&source, None).await.unwrap();

        // Guard against the assertion below passing for the wrong reason: the
        // checkout the archive was built from really does have a `.git` dir.
        let checkout = checkout_dir
            .path()
            .join(sanitize_cache_key(&source.cache_key()));
        assert!(
            checkout.join(".git").is_dir(),
            "fixture checkout has no git metadata to exclude"
        );

        let entries = archive_entry_paths(&archive);
        assert!(
            entries.iter().any(|path| path == "bar/PKGBUILD"),
            "sources are missing from the archive: {entries:?}"
        );
        assert!(
            !entries
                .iter()
                .any(|path| path == "bar/.git" || path.starts_with("bar/.git/")),
            "archive must not carry git metadata: {entries:?}"
        );
    }

    /// Requests for one uncached source arriving together all succeed. Each
    /// clones or fetches into the same checkout directory, which git cannot
    /// do twice at once; the checkout lock queues them instead.
    #[tokio::test]
    async fn concurrent_first_reads_of_one_source_all_succeed() {
        let aur_root = tempfile::tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "bar", "1.0");
        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let store = Arc::new(store);
        let source = SourceData::Aur {
            name: "bar".to_string(),
        };

        let reads: Vec<_> = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                let source = source.clone();
                tokio::spawn(async move { store.refresh(&source).await })
            })
            .collect();
        for read in reads {
            read.await.unwrap().expect("a concurrent read failed");
        }
    }

    /// Regression test for a bug introduced (and fixed) while reworking the
    /// cache to hold a single entry per source: `list_files`/`read_file`
    /// must reuse whatever is already cached for a source - regardless of
    /// whether a patch happens to be active for it - rather than treating a
    /// patch-vs-no-patch mismatch as a cache miss and forcing a redundant
    /// re-fetch from the (in this test, local-only) git remote.
    #[tokio::test]
    async fn list_files_and_read_file_reuse_cache_regardless_of_active_patch() {
        if !pkgbuild_bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }

        let aur_root = tempfile::tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "bar", "1.0");
        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let source = SourceData::Aur {
            name: "bar".to_string(),
        };

        // Prime the cache with a *patched* entry (active = patched content).
        let patch = some_patch("2.0");
        let patch_json = patch.to_json().unwrap();
        let sourceinfo = store.sourceinfo(&source, Some(&patch_json)).await.unwrap();
        assert_eq!(sourceinfo.base.version.to_string(), "2.0-1");

        // Once the upstream git remote is gone, any code path that would
        // force a re-fetch (rather than reusing the cached entry) fails
        // here - proving list_files/read_file only ever reuse the cache.
        std::fs::remove_dir_all(aur_root.path().join("bar.git")).unwrap();

        let files = store.list_files(&source).await.unwrap();
        assert!(files.contains(&"PKGBUILD".to_string()));

        // read_file(patch=None) must return the *pristine* content (pkgver
        // 1.0), not the currently-active patched content (pkgver 2.0),
        // proving `original` was correctly preserved alongside `active`.
        let pristine = store.read_file(&source, None, "PKGBUILD").await.unwrap();
        assert!(pristine.contains("pkgver=1.0"));
        assert!(!pristine.contains("pkgver=2.0"));

        // read_file with the same patch re-applied must return the patched
        // content, computed from the still-cached pristine original.
        let patched = store
            .read_file(&source, Some(&patch_json), "PKGBUILD")
            .await
            .unwrap();
        assert!(patched.contains("pkgver=2.0"));
    }

    /// There is at most one cache entry per source at a time: switching from
    /// no patch, to a patch, and back to no patch must each fully replace
    /// the previous entry rather than accumulating stale variants that could
    /// be read back by mistake.
    #[tokio::test]
    async fn at_most_one_cache_entry_per_source_across_patch_changes() {
        if !pkgbuild_bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }

        let aur_root = tempfile::tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "bar", "1.0");
        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let source = SourceData::Aur {
            name: "bar".to_string(),
        };

        // No patch: active == pristine.
        let unpatched = store.sourceinfo(&source, None).await.unwrap();
        assert_eq!(unpatched.base.version.to_string(), "1.0-1");
        assert_eq!(store.cache.lock().await.len(), 1);

        // Apply patch A: single entry now reflects patch A.
        let patch_a = some_patch("2.0").to_json().unwrap();
        let a = store.sourceinfo(&source, Some(&patch_a)).await.unwrap();
        assert_eq!(a.base.version.to_string(), "2.0-1");
        assert_eq!(store.cache.lock().await.len(), 1);

        // Switch to patch B: the entry for patch A must be gone - reading
        // with patch B must not somehow see stale patch-A content, and only
        // one entry must exist for this source.
        let patch_b = some_patch("3.0").to_json().unwrap();
        let b = store.sourceinfo(&source, Some(&patch_b)).await.unwrap();
        assert_eq!(b.base.version.to_string(), "3.0-1");
        assert_eq!(store.cache.lock().await.len(), 1);

        // Clearing the patch must revert to the pristine content, not
        // whatever the last-active patch happened to produce.
        let cleared = store.sourceinfo(&source, None).await.unwrap();
        assert_eq!(cleared.base.version.to_string(), "1.0-1");
        assert_eq!(store.cache.lock().await.len(), 1);
    }

    /// A new patch is applied to the pristine snapshot already held: keeping
    /// up with upstream is `refresh`'s job, so editing a patch never waits on
    /// the remote -- here, one that is gone.
    #[tokio::test]
    async fn changing_the_patch_does_not_fetch_again() {
        if !pkgbuild_bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }

        let aur_root = tempfile::tempdir().unwrap();
        let remote = create_aur_git_repo(aur_root.path(), "bar", "1.0");
        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let source = SourceData::Aur {
            name: "bar".to_string(),
        };
        store.sourceinfo(&source, None).await.unwrap();

        std::fs::remove_dir_all(remote).unwrap();
        let patch = some_patch("2.0").to_json().unwrap();
        let patched = store.sourceinfo(&source, Some(&patch)).await.unwrap();
        assert_eq!(patched.base.version.to_string(), "2.0-1");
    }

    /// `refresh` must re-apply whichever patch was active before the
    /// refresh, rather than silently reverting the source to unpatched.
    #[tokio::test]
    async fn refresh_reapplies_previously_active_patch() {
        if !pkgbuild_bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }

        // Use a plain Git source (rather than an AUR one) so the "main"
        // branch ref reliably reflects newly pushed commits, matching the
        // pattern used elsewhere in this crate for exercising
        // `checkout_or_fetch_repo_ref`'s fetch-and-update behavior.
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(repo_dir.path()).unwrap();
        commit_to_repo(&repo, "v1", "init");

        let TestStore { store, _checkouts } = test_store(Path::new("unused"));
        let source = SourceData::Git {
            spec: aurcache_db::packages::GitSourceSpec {
                url: repo_dir.path().to_string_lossy().to_string(),
                r#ref: "origin/main".to_string(),
                subfolder: String::new(),
            },
        };

        let patch = some_patch("2.0").to_json().unwrap();
        store.sourceinfo(&source, Some(&patch)).await.unwrap();

        // Upstream moves to a new commit (still parseable as version 1.0,
        // since the patch bumps it to 2.0 - if refresh dropped the patch,
        // this would come back as 1.0 instead of 2.0).
        commit_to_repo(&repo, "v2", "bump");

        let refreshed = store.refresh(&source).await.unwrap();
        assert_eq!(
            refreshed,
            Refresh::Changed,
            "refresh should detect the new upstream commit"
        );

        let after_refresh = store.sourceinfo(&source, Some(&patch)).await.unwrap();
        assert_eq!(after_refresh.base.version.to_string(), "2.0-1");
    }

    /// `refresh` on an AUR source must pick up a PKGBUILD bump committed
    /// after the snapshot was cached.
    #[tokio::test]
    async fn refresh_detects_an_aur_bump() {
        if !pkgbuild_bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }

        let aur_root = tempfile::tempdir().unwrap();
        let repo_path = create_aur_git_repo(aur_root.path(), "foo", "1.0");
        let TestStore { store, _checkouts } = test_store(aur_root.path());
        let source = SourceData::Aur {
            name: "foo".to_string(),
        };

        let first = store.sourceinfo(&source, None).await.unwrap();
        assert_eq!(first.base.version.to_string(), "1.0-1");

        let repo = Repository::open(&repo_path).unwrap();
        std::fs::write(
            repo_path.join("PKGBUILD"),
            "pkgname=foo\npkgver=1.1\npkgrel=1\narch=('x86_64')\ndepends=()\nsource=()\nsha256sums=()\npackage() {\n  :\n}\n",
        )
        .unwrap();
        std::fs::write(
            repo_path.join(".SRCINFO"),
            "pkgbase = foo\n\tpkgver = 1.1\n\tpkgrel = 1\n\narch = x86_64\n\npkgname = foo\n",
        )
        .unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.add_path(Path::new(".SRCINFO")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "bump", &tree, &[&parent])
            .unwrap();

        let refreshed = store.refresh(&source).await.unwrap();
        assert_eq!(
            refreshed,
            Refresh::Changed,
            "refresh should detect the bumped PKGBUILD"
        );

        let second = store.sourceinfo(&source, None).await.unwrap();
        assert_eq!(second.base.version.to_string(), "1.1-1");
    }

    /// A deleted package's clone must not outlive it.
    #[tokio::test]
    async fn remove_checkout_takes_the_directory_and_the_cached_entry() {
        let aur_root = tempfile::tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "foo", "1.0");
        let TestStore {
            store,
            _checkouts: checkout_dir,
        } = test_store(aur_root.path());
        let source = SourceData::Aur {
            name: "foo".to_string(),
        };

        store.sourceinfo(&source, None).await.unwrap();
        let path = checkout_dir
            .path()
            .join(sanitize_cache_key(&source.cache_key()));
        assert!(path.is_dir(), "the fetch should have left a checkout");
        assert!(store.cache.lock().await.contains(&source.cache_key()));

        store.remove_checkout(&source, &[]).await.unwrap();

        assert!(!path.exists(), "the checkout outlived the package");
        assert!(
            !store.cache.lock().await.contains(&source.cache_key()),
            "the snapshot outlived the package"
        );
    }

    /// Deleting a package that was never resolved has no checkout to remove,
    /// and that is not a failure.
    #[tokio::test]
    async fn remove_checkout_is_a_noop_when_there_is_nothing_to_remove() {
        let aur_root = tempfile::tempdir().unwrap();
        let TestStore { store, _checkouts } = test_store(aur_root.path());

        store
            .remove_checkout(
                &SourceData::Aur {
                    name: "never-fetched".to_string(),
                },
                &[],
            )
            .await
            .unwrap();
    }

    /// Cache keys are not unique per package -- every upload shares one -- so
    /// a checkout a remaining package maps to stays.
    #[tokio::test]
    async fn remove_checkout_keeps_a_directory_a_remaining_package_shares() {
        let aur_root = tempfile::tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "foo", "1.0");
        let TestStore {
            store,
            _checkouts: checkout_dir,
        } = test_store(aur_root.path());
        let source = SourceData::Aur {
            name: "foo".to_string(),
        };
        store.sourceinfo(&source, None).await.unwrap();
        let path = checkout_dir
            .path()
            .join(sanitize_cache_key(&source.cache_key()));

        store
            .remove_checkout(&source, std::slice::from_ref(&source))
            .await
            .unwrap();

        assert!(path.is_dir(), "a checkout still claimed was removed");
    }

    /// The case no delete path can reach: a checkout whose package was never
    /// written, as an add that fails part-way through leaves behind.
    #[tokio::test]
    async fn prune_removes_only_the_checkouts_no_source_claims() {
        let aur_root = tempfile::tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "kept", "1.0");
        create_aur_git_repo(aur_root.path(), "stranded", "1.0");
        let TestStore {
            store,
            _checkouts: checkout_dir,
        } = test_store(aur_root.path());

        let kept = SourceData::Aur {
            name: "kept".to_string(),
        };
        let stranded = SourceData::Aur {
            name: "stranded".to_string(),
        };
        store.sourceinfo(&kept, None).await.unwrap();
        store.sourceinfo(&stranded, None).await.unwrap();

        // A file rather than a directory: not this store's, so not its business.
        let stray = checkout_dir.path().join("notes.txt");
        std::fs::write(&stray, "not a checkout").unwrap();

        let removed = store
            .prune_orphaned_checkouts(std::slice::from_ref(&kept))
            .await
            .unwrap();

        assert_eq!(removed, 1);
        assert!(
            checkout_dir
                .path()
                .join(sanitize_cache_key(&kept.cache_key()))
                .is_dir(),
            "pruned a checkout a live package still needs"
        );
        assert!(
            !checkout_dir
                .path()
                .join(sanitize_cache_key(&stranded.cache_key()))
                .exists()
        );
        assert!(stray.exists(), "removed something that is not a checkout");
    }

    /// Nothing has been checked out yet, so nothing can be orphaned -- the
    /// state every instance is in on its first boot.
    #[tokio::test]
    async fn prune_tolerates_a_checkout_root_that_does_not_exist() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root(root.path().join("never-created"));

        assert_eq!(store.prune_orphaned_checkouts(&[]).await.unwrap(), 0);
    }
}
