use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use alpm_srcinfo::SourceInfoV1;
use aurcache_db::packages::SourceData;
use aurcache_deps::AurClient;
use git2::Oid;
use tokio::sync::Mutex;

use crate::pkgbuild::fix_source_urls;

/// Base URL for AUR git repositories. AUR packages are unified with git
/// sources: `https://aur.archlinux.org/{pkgbase}.git`, ref `HEAD`, no
/// subfolder. The AUR RPC (`AurClient`) is only used for metadata/dependency
/// resolution, not for fetching sources.
fn default_aur_git_base_url() -> String {
    "https://aur.archlinux.org".to_string()
}

struct CacheEntry {
    sourceinfo: Arc<SourceInfoV1>,
    archive_bytes: Arc<Vec<u8>>,
    /// Resolved commit id last used to build this entry, used to detect
    /// whether a `refresh` actually changed anything.
    commit: Oid,
}

/// Directory under which persistent git checkouts are kept, one subdirectory
/// per `SourceData::cache_key()`. Reusing these clones across calls means a
/// `refresh` only needs to `git fetch` (transfer new objects) instead of a
/// full re-clone.
fn default_checkout_root() -> PathBuf {
    std::env::var("AURCACHE_SOURCE_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./source_cache"))
}

/// Cache for checked-out source repositories and their parsed `.SRCINFO`.
///
/// A single instance should be passed around so that repeated requests for the
/// same source return the cached result. Backed by a persistent on-disk git
/// checkout per source (see `default_checkout_root`), so repeat fetches are
/// cheap incremental `git fetch`s rather than full clones/downloads.
pub struct SnapshotStore {
    cache: Mutex<HashMap<String, Arc<CacheEntry>>>,
    checkout_root: PathBuf,
    aur_git_base_url: String,
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
        SnapshotStore {
            cache: Mutex::new(HashMap::new()),
            checkout_root,
            aur_git_base_url: default_aur_git_base_url(),
        }
    }

    /// Construct a store with an explicit checkout root and AUR git base URL
    /// (e.g. `file:///tmp/fake-aur` in tests, instead of the real AUR).
    pub fn with_checkout_root_and_aur_base(
        checkout_root: PathBuf,
        aur_git_base_url: impl Into<String>,
    ) -> Self {
        SnapshotStore {
            cache: Mutex::new(HashMap::new()),
            checkout_root,
            aur_git_base_url: aur_git_base_url.into(),
        }
    }

    /// Return the parsed `.SRCINFO` for `source_data`, fetching it if not cached.
    pub async fn sourceinfo(
        &self,
        client: &AurClient,
        source_data: &SourceData,
    ) -> anyhow::Result<SourceInfoV1> {
        let entry = self.get_or_fetch(client, source_data).await?;
        Ok((*entry.sourceinfo).clone())
    }

    /// Return the raw archive bytes for `source_data`, fetching it if not cached.
    pub async fn archive_bytes(
        &self,
        client: &AurClient,
        source_data: &SourceData,
    ) -> anyhow::Result<Vec<u8>> {
        let entry = self.get_or_fetch(client, source_data).await?;
        Ok((*entry.archive_bytes).clone())
    }

    /// Proactively refresh the cache entry for `source_data`: fetch the
    /// latest state from the remote and, if the resolved ref actually moved
    /// (or there was no cached entry yet), re-parse/re-tar it. Returns `true`
    /// if the entry changed (i.e. the source is not up to date with what was
    /// previously cached), `false` if it was already current.
    ///
    /// This is intended to be called from the periodic version-check loop so
    /// that staleness is detected (and long-lived caches kept honest) without
    /// unconditionally re-downloading/re-cloning on every check.
    pub async fn refresh(
        &self,
        client: &AurClient,
        source_data: &SourceData,
    ) -> anyhow::Result<bool> {
        let cache_key = source_data.cache_key();
        let previous_commit = {
            let cache = self.cache.lock().await;
            cache.get(&cache_key).map(|entry| entry.commit)
        };

        let (repo_url, git_ref, subfolder) = self.git_coordinates(source_data)?;
        let path = self.checkout_root.join(sanitize_cache_key(&cache_key));

        let (commit, archive_bytes, sourceinfo) =
            checkout_and_parse(&repo_url, &git_ref, &subfolder, &path).await?;

        let changed = previous_commit != Some(commit);
        if changed {
            let entry = Arc::new(CacheEntry {
                sourceinfo: Arc::new(sourceinfo),
                archive_bytes: Arc::new(archive_bytes),
                commit,
            });
            self.cache.lock().await.insert(cache_key, entry);
        }
        let _ = client; // reserved for future use (e.g. AUR metadata cross-check)
        Ok(changed)
    }

    async fn get_or_fetch(
        &self,
        client: &AurClient,
        source_data: &SourceData,
    ) -> anyhow::Result<Arc<CacheEntry>> {
        let cache_key = source_data.cache_key();

        // Fast path: already cached
        {
            let cache = self.cache.lock().await;
            if let Some(entry) = cache.get(&cache_key) {
                return Ok(entry.clone());
            }
        }

        let (repo_url, git_ref, subfolder) = self.git_coordinates(source_data)?;
        let path = self.checkout_root.join(sanitize_cache_key(&cache_key));
        let (commit, archive_bytes, sourceinfo) =
            checkout_and_parse(&repo_url, &git_ref, &subfolder, &path).await?;

        let entry = Arc::new(CacheEntry {
            sourceinfo: Arc::new(sourceinfo),
            archive_bytes: Arc::new(archive_bytes),
            commit,
        });

        self.cache.lock().await.insert(cache_key, entry.clone());
        let _ = client;
        Ok(entry)
    }

    /// Map a `SourceData` to the git coordinates used to fetch it: repo URL,
    /// ref, and subfolder within the repo containing the PKGBUILD/.SRCINFO.
    fn git_coordinates(
        &self,
        source_data: &SourceData,
    ) -> anyhow::Result<(String, String, String)> {
        match source_data {
            SourceData::Aur { name } => Ok((
                format!("{}/{name}.git", self.aur_git_base_url),
                "HEAD".to_string(),
                String::new(),
            )),
            SourceData::Git { spec } => {
                Ok((spec.url.clone(), spec.r#ref.clone(), spec.subfolder.clone()))
            }
            SourceData::Upload { .. } => anyhow::bail!("Upload sources are not yet supported"),
        }
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

/// Checkout (or fetch-and-update) `repo_url`@`git_ref` into the persistent
/// `path`, then parse the `.SRCINFO`/PKGBUILD and build a tar.gz archive of
/// `subfolder` (or the repo root if empty) with `{pkgbase}/` as the
/// top-level directory, matching the structure of AUR snapshots.
async fn checkout_and_parse(
    repo_url: &str,
    git_ref: &str,
    subfolder: &str,
    path: &std::path::Path,
) -> anyhow::Result<(Oid, Vec<u8>, SourceInfoV1)> {
    use crate::git::checkout::checkout_or_fetch_repo_ref;
    use crate::pkgbuild::parse_pkgbuild;

    let repo_url = repo_url.to_string();
    let git_ref = git_ref.to_string();
    let path = path.to_path_buf();
    let subfolder = subfolder.to_string();

    // git2 types are not `Send`, so the checkout itself must run fully
    // within the blocking closure.
    let (commit, package_dir) = tokio::task::spawn_blocking(move || {
        let commit = checkout_or_fetch_repo_ref(&repo_url, &git_ref, &path)?;
        let package_dir = if subfolder.is_empty() {
            path.clone()
        } else {
            path.join(&subfolder)
        };
        anyhow::Ok((commit, package_dir))
    })
    .await??;

    let srcinfo_path = package_dir.join(".SRCINFO");
    let sourceinfo = if srcinfo_path.exists() {
        let content = std::fs::read_to_string(&srcinfo_path)?;
        SourceInfoV1::from_string(&fix_source_urls(&content))?
    } else {
        parse_pkgbuild(package_dir.join("PKGBUILD").as_path())?
    };

    let pkgbase = sourceinfo.base.name.to_string();
    let tar_gz_bytes = create_archive_with_pkgbase_dir(&package_dir, &pkgbase)?;

    Ok((commit, tar_gz_bytes, sourceinfo))
}

fn create_archive_with_pkgbase_dir(
    source_dir: &std::path::Path,
    pkgbase: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(pkgbase, source_dir)?;
    let enc = tar.into_inner()?;
    drop(enc);
    Ok(buf)
}
