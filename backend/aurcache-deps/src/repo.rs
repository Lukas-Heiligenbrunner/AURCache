use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;

use alpm_compress::tarball::TarballReader;
use alpm_repo_db::desc::RepoDescFile;
use backon::{FibonacciBuilder, Retryable};
use url::Url;

use crate::client::AurClient;
use crate::deps::parse_dep;
use crate::model::{Error, OfficialPackage, OfficialPackageResponse};

const OFFICIAL_REPO_NAMES: &[&str] = &["core", "extra", "multilib"];
const OFFICIAL_REPO_CACHE_TTL_SECS: u64 = 60 * 60;

pub(crate) fn official_packages_url_for_aur(rpc_url: &str) -> String {
    if let Ok(url) = std::env::var("ARCH_PACKAGES_API_URL") {
        return url;
    }

    if rpc_url.starts_with("https://aur.archlinux.org/") {
        return "https://archlinux.org/packages/search/json/".to_string();
    }

    let Ok(mut url) = Url::parse(rpc_url) else {
        return "https://archlinux.org/packages/search/json/".to_string();
    };
    url.set_path("/packages/search/json/");
    url.set_query(None);
    url.into()
}

pub(crate) fn default_repo_root() -> PathBuf {
    std::env::var("AURCACHE_REPO_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./repo"))
}

pub(crate) fn default_official_mirrorlist_path() -> PathBuf {
    if let Ok(path) = std::env::var("OFFICIAL_MIRRORLIST_PATH") {
        return PathBuf::from(path);
    }

    let base = std::env::var("MIRRORLIST_PATH_X86_64")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./config/pacman_x86_64"));
    base.join("mirrorlist")
}

pub(crate) fn default_official_repo_cache_dir() -> PathBuf {
    if let Ok(path) = std::env::var("OFFICIAL_REPO_CACHE_DIR") {
        return PathBuf::from(path);
    }

    default_official_mirrorlist_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("official_repo_cache")
}

impl AurClient {
    pub(crate) fn local_repo_dependency_exists(&self, dep_name: &str) -> Result<bool, Error> {
        if !self.repo_root.exists() {
            return Ok(false);
        }

        for entry in fs::read_dir(&self.repo_root).map_err(|e| Error::Rpc(e.to_string()))? {
            let entry = entry.map_err(|e| Error::Rpc(e.to_string()))?;
            let archive_path = entry.path().join("repo.db.tar.gz");
            if !archive_path.exists() {
                continue;
            }

            if repo_archive_provides(&archive_path, dep_name)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub(crate) async fn official_search(&self, query: &str) -> Result<Vec<OfficialPackage>, Error> {
        let url = self.official_search_url(query)?;
        let http = self.http.clone();
        let fetch = move || {
            let http = http.clone();
            let url = url.clone();
            async move { http.get(url).send().await }
        };
        let resp = fetch
            .retry(
                FibonacciBuilder::default()
                    .with_min_delay(std::time::Duration::from_millis(500))
                    .with_max_times(3),
            )
            .await
            .map_err(Error::Http)?
            .error_for_status()
            .map_err(Error::Http)?;
        let text = resp.text().await?;
        let response: OfficialPackageResponse =
            serde_json::from_str(&text).map_err(|e| Error::Rpc(e.to_string()))?;
        Ok(response.results)
    }

    pub(crate) async fn cached_official_dependency_exists(
        &self,
        dep_name: &str,
    ) -> Result<bool, Error> {
        self.refresh_official_repo_cache_if_needed().await?;
        for repo_name in OFFICIAL_REPO_NAMES {
            let archive_path = self
                .official_repo_cache_dir
                .join(cache_file_name(repo_name));
            if !archive_path.exists() {
                continue;
            }
            if repo_archive_provides(&archive_path, dep_name)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn refresh_official_repo_cache_if_needed(&self) -> Result<(), Error> {
        fs::create_dir_all(&self.official_repo_cache_dir).map_err(|e| Error::Rpc(e.to_string()))?;
        let mirrors = mirror_servers(&self.official_mirrorlist_path)?;
        if mirrors.is_empty() {
            return Err(Error::Rpc(
                "No official repo mirrors configured".to_string(),
            ));
        }

        for repo_name in OFFICIAL_REPO_NAMES {
            let archive_path = self
                .official_repo_cache_dir
                .join(cache_file_name(repo_name));
            if !cache_is_stale(&archive_path)? {
                continue;
            }
            self.download_official_repo_db(&mirrors, repo_name, &archive_path)
                .await?;
        }

        Ok(())
    }

    async fn download_official_repo_db(
        &self,
        mirrors: &[String],
        repo_name: &str,
        archive_path: &Path,
    ) -> Result<(), Error> {
        let mut last_error = None;
        for mirror in mirrors {
            let url = official_repo_db_url(mirror, repo_name)?;
            match self.download_to_path(&url, archive_path).await {
                Ok(()) => return Ok(()),
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error
            .unwrap_or_else(|| Error::Rpc("Failed to download official repo db".to_string())))
    }

    async fn download_to_path(&self, url: &Url, archive_path: &Path) -> Result<(), Error> {
        let bytes = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(Error::Http)?
            .error_for_status()
            .map_err(Error::Http)?
            .bytes()
            .await
            .map_err(Error::Http)?;
        fs::write(archive_path, bytes).map_err(|e| Error::Rpc(e.to_string()))?;
        Ok(())
    }
}

fn cache_is_stale(path: &Path) -> Result<bool, Error> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(Error::Rpc(err.to_string())),
    };
    let modified = metadata.modified().map_err(|e| Error::Rpc(e.to_string()))?;
    let age = SystemTime::now()
        .duration_since(modified)
        .map_err(|e| Error::Rpc(e.to_string()))?;
    Ok(age.as_secs() > OFFICIAL_REPO_CACHE_TTL_SECS)
}

fn mirror_servers(path: &Path) -> Result<Vec<String>, Error> {
    let content = fs::read_to_string(path).map_err(|e| Error::Rpc(e.to_string()))?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("Server = "))
        .map(str::trim)
        .map(ToString::to_string)
        .collect())
}

fn official_repo_db_url(mirror: &str, repo_name: &str) -> Result<Url, Error> {
    let base = mirror
        .replace("$repo", repo_name)
        .replace("$arch", "x86_64");
    let separator = if base.ends_with('/') { "" } else { "/" };
    Url::parse(&format!("{base}{separator}{repo_name}.db")).map_err(|e| Error::Rpc(e.to_string()))
}

fn cache_file_name(repo_name: &str) -> String {
    format!("{repo_name}.db.tar.gz")
}

fn repo_archive_provides(archive_path: &Path, dep_name: &str) -> Result<bool, Error> {
    let mut reader =
        TarballReader::try_from(archive_path).map_err(|e| Error::Rpc(e.to_string()))?;
    for entry in reader.entries().map_err(|e| Error::Rpc(e.to_string()))? {
        let mut entry = entry.map_err(|e| Error::Rpc(e.to_string()))?;
        if entry.path().file_name().and_then(|name| name.to_str()) != Some("desc") {
            continue;
        }

        let content = String::from_utf8(entry.content().map_err(|e| Error::Rpc(e.to_string()))?)
            .map_err(|e| Error::Rpc(e.to_string()))?;
        let desc = RepoDescFile::from_str(&content).map_err(|e| Error::Rpc(e.to_string()))?;
        if repo_desc_matches_dependency(&desc, dep_name) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn repo_desc_matches_dependency(desc: &RepoDescFile, dep_name: &str) -> bool {
    match desc {
        RepoDescFile::V1(file) => {
            file.name.to_string() == dep_name
                || file
                    .provides
                    .iter()
                    .any(|provide| parse_dep(&provide.to_string()).0 == dep_name)
        }
        RepoDescFile::V2(file) => {
            file.name.to_string() == dep_name
                || file
                    .provides
                    .iter()
                    .any(|provide| parse_dep(&provide.to_string()).0 == dep_name)
        }
    }
}
