use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use alpm_srcinfo::SourceInfoV1;
use aurcache_db::packages::{GitSourceSpec, SourceData};
use aurcache_deps::AurClient;
use tokio::sync::Mutex;

use crate::pkgbuild::fix_source_urls;

struct CacheEntry {
    sourceinfo: Arc<SourceInfoV1>,
    archive_bytes: Arc<Vec<u8>>,
}

/// Cache for downloaded/checked-out source archives and their parsed `.SRCINFO`.
///
/// A single instance should be passed around so that repeated requests for the
/// same source return the cached result.
pub struct SnapshotStore {
    cache: Mutex<HashMap<String, Arc<CacheEntry>>>,
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotStore {
    pub fn new() -> Self {
        SnapshotStore {
            cache: Mutex::new(HashMap::new()),
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

        let (archive_bytes, sourceinfo) = fetch_source(client, source_data).await?;

        let entry = Arc::new(CacheEntry {
            sourceinfo: Arc::new(sourceinfo),
            archive_bytes: Arc::new(archive_bytes),
        });

        self.cache.lock().await.insert(cache_key, entry.clone());
        Ok(entry)
    }
}

async fn fetch_source(
    client: &AurClient,
    source_data: &SourceData,
) -> anyhow::Result<(Vec<u8>, SourceInfoV1)> {
    match source_data {
        SourceData::Aur { name } => fetch_aur_source(client, name).await,
        SourceData::Git { spec } => fetch_git_source(spec),
        SourceData::Upload { .. } => anyhow::bail!("Upload sources are not yet supported"),
    }
}

async fn fetch_aur_source(
    client: &AurClient,
    pkgbase: &str,
) -> anyhow::Result<(Vec<u8>, SourceInfoV1)> {
    let archive_bytes = client
        .download_snapshot_bytes(pkgbase)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to download AUR snapshot for {pkgbase}: {e}"))?;
    let sourceinfo = parse_srcinfo_from_tar_gz(&archive_bytes, pkgbase)?;
    Ok((archive_bytes, sourceinfo))
}

fn fetch_git_source(spec: &GitSourceSpec) -> anyhow::Result<(Vec<u8>, SourceInfoV1)> {
    use crate::git::checkout::checkout_git_source;
    use crate::pkgbuild::parse_pkgbuild;

    let dir = tempfile::tempdir()?;
    let repo_path = dir.path().join("repo");
    checkout_git_source(spec, repo_path.clone())?;

    let package_dir = repo_path.join(&spec.subfolder);
    let srcinfo_path = package_dir.join(".SRCINFO");
    let sourceinfo = if srcinfo_path.exists() {
        let content = std::fs::read_to_string(srcinfo_path)?;
        SourceInfoV1::from_string(&fix_source_urls(&content))?
    } else {
        parse_pkgbuild(package_dir.join("PKGBUILD").as_path())?
    };

    let pkgbase = sourceinfo.base.name.to_string();

    // Create a tar.gz with {pkgbase}/ as the top-level directory,
    // matching the structure of AUR snapshots.
    let tar_gz_bytes = create_archive_with_pkgbase_dir(&package_dir, &pkgbase)?;

    dir.close()?;
    Ok((tar_gz_bytes, sourceinfo))
}

fn parse_srcinfo_from_tar_gz(archive_bytes: &[u8], pkgbase: &str) -> anyhow::Result<SourceInfoV1> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);

    let mut srcinfo_str = None;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        if path == format!("{pkgbase}/.SRCINFO") || path.ends_with("/.SRCINFO") {
            let mut buf = String::new();
            entry.read_to_string(&mut buf)?;
            srcinfo_str = Some(buf);
            break;
        }
    }

    let srcinfo_str = srcinfo_str
        .ok_or_else(|| anyhow::anyhow!(".SRCINFO not found in AUR snapshot for {pkgbase}"))?;

    Ok(SourceInfoV1::from_string(&fix_source_urls(&srcinfo_str))?)
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
