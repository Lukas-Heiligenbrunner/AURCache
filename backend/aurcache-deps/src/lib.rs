use std::collections::{HashMap, HashSet};
use std::fs::{self};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use alpm_compress::tarball::TarballReader;
use alpm_repo_db::desc::RepoDescFile;
use alpm_srcinfo::SourceInfoV1;
use backon::{FibonacciBuilder, Retryable};
use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum Error {
    #[error("package not found: {0}")]
    NotFound(String),
    #[error("AUR RPC error: {0}")]
    Rpc(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Clone)]
pub struct AurClient {
    http: Client,
    rpc_url: String,
    official_packages_url: String,
    repo_root: PathBuf,
    official_mirrorlist_path: PathBuf,
    official_repo_cache_dir: PathBuf,
}

impl Default for AurClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a snapshot download URL for an AUR package from the RPC URL.
/// Strips `/rpc/v5` from the RPC URL to derive the base domain (if present).
pub fn snapshot_url(rpc_url: &str, pkgbase: &str) -> String {
    let base = rpc_url.trim_end_matches("/rpc/v5").trim_end_matches('/');
    format!("{base}/cgit/aur.git/snapshot/{pkgbase}.tar.gz")
}

impl AurClient {
    /// Construct a new client from the `AUR_RPC_URL` env var (or default).
    pub fn new() -> Self {
        let rpc_url = std::env::var("AUR_RPC_URL")
            .unwrap_or_else(|_| "https://aur.archlinux.org/rpc/v5".to_string());
        Self {
            http: Client::new(),
            official_packages_url: official_packages_url_for_aur(&rpc_url),
            repo_root: default_repo_root(),
            official_mirrorlist_path: default_official_mirrorlist_path(),
            official_repo_cache_dir: default_official_repo_cache_dir(),
            rpc_url,
        }
    }

    pub fn with_aur_url(aur_url: impl Into<String>) -> Self {
        let rpc_url = aur_url.into();
        Self {
            http: Client::new(),
            official_packages_url: official_packages_url_for_aur(&rpc_url),
            repo_root: default_repo_root(),
            official_mirrorlist_path: default_official_mirrorlist_path(),
            official_repo_cache_dir: default_official_repo_cache_dir(),
            rpc_url,
        }
    }

    pub fn with_urls(aur_url: impl Into<String>, official_packages_url: impl Into<String>) -> Self {
        Self::with_urls_and_paths(
            aur_url,
            official_packages_url,
            default_repo_root(),
            default_official_mirrorlist_path(),
            default_official_repo_cache_dir(),
        )
    }

    pub fn with_urls_and_repo_root(
        aur_url: impl Into<String>,
        official_packages_url: impl Into<String>,
        repo_root: impl Into<PathBuf>,
    ) -> Self {
        Self::with_urls_and_paths(
            aur_url,
            official_packages_url,
            repo_root,
            default_official_mirrorlist_path(),
            default_official_repo_cache_dir(),
        )
    }

    pub fn with_urls_and_paths(
        aur_url: impl Into<String>,
        official_packages_url: impl Into<String>,
        repo_root: impl Into<PathBuf>,
        official_mirrorlist_path: impl Into<PathBuf>,
        official_repo_cache_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            http: Client::new(),
            rpc_url: aur_url.into(),
            official_packages_url: official_packages_url.into(),
            repo_root: repo_root.into(),
            official_mirrorlist_path: official_mirrorlist_path.into(),
            official_repo_cache_dir: official_repo_cache_dir.into(),
        }
    }

    fn rpc_info_url(&self, args: &[&str]) -> Result<Url, Error> {
        let mut url =
            Url::parse(&format!("{}/info", self.rpc_url)).map_err(|e| Error::Rpc(e.to_string()))?;
        for arg in args {
            url.query_pairs_mut().append_pair("arg[]", arg);
        }
        Ok(url)
    }

    fn official_search_url(&self, query: &str) -> Result<Url, Error> {
        let mut url =
            Url::parse(&self.official_packages_url).map_err(|e| Error::Rpc(e.to_string()))?;
        url.query_pairs_mut().append_pair("q", query);
        Ok(url)
    }
}

#[derive(Debug, Clone)]
pub struct PkgDeps {
    pub depends: Vec<String>,
    pub make_depends: Vec<String>,
    pub pkgnames: Vec<String>,
    pub provides: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyResolution {
    Official,
    Local { pkgbase: String },
    Aur { pkgbase: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Description")]
    pub description: Option<String>,
    #[serde(rename = "Maintainer")]
    pub maintainer: Option<String>,
    #[serde(rename = "URL")]
    pub url: Option<String>,
    #[serde(rename = "NumVotes")]
    pub num_votes: u32,
    #[serde(rename = "Popularity")]
    pub popularity: f32,
    #[serde(rename = "OutOfDate")]
    pub out_of_date: Option<u32>,
    #[serde(rename = "PackageBase")]
    pub package_base: String,
    #[serde(rename = "PackageBaseID")]
    pub package_base_id: u32,
    #[serde(rename = "FirstSubmitted")]
    pub first_submitted: u32,
    #[serde(rename = "LastModified")]
    pub last_modified: u32,
    #[serde(rename = "URLPath")]
    pub url_path: Option<String>,
    #[serde(rename = "ID")]
    pub id: u32,
    #[serde(rename = "Depends", default)]
    pub depends: Option<Vec<String>>,
    #[serde(rename = "MakeDepends", default)]
    pub make_depends: Option<Vec<String>>,
    #[serde(rename = "OptDepends", default)]
    pub opt_depends: Option<Vec<String>>,
    #[serde(rename = "CheckDepends", default)]
    pub check_depends: Option<Vec<String>>,
    #[serde(rename = "Conflicts", default)]
    pub conflicts: Option<Vec<String>>,
    #[serde(rename = "Provides", default)]
    pub provides: Option<Vec<String>>,
    #[serde(rename = "Replaces", default)]
    pub replaces: Option<Vec<String>>,
    #[serde(rename = "Groups", default)]
    pub groups: Option<Vec<String>>,
    #[serde(rename = "License", default)]
    pub license: Option<Vec<String>>,
    #[serde(rename = "Keywords", default)]
    pub keywords: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct PackageResponse {
    #[serde(rename = "type")]
    response_type: String,
    #[serde(default)]
    error: Option<String>,
    results: Vec<Package>,
}

#[derive(Debug, Clone, Deserialize)]
struct OfficialPackage {
    pkgname: String,
    #[serde(default)]
    provides: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OfficialPackageResponse {
    results: Vec<OfficialPackage>,
}

impl AurClient {
    pub async fn resolve_bases(&self, names: &[&str]) -> Result<HashMap<String, String>, Error> {
        let packages = self.rpc_fetch(self.rpc_info_url(names)?).await?;
        Ok(packages
            .into_iter()
            .map(|pkg| (pkg.name, pkg.package_base))
            .collect())
    }

    pub async fn deps_of(&self, pkgbase: &str) -> Result<PkgDeps, Error> {
        self.deps_of_rpc(pkgbase).await
    }

    pub async fn info_of(&self, name: &str) -> Result<Option<Package>, Error> {
        let mut packages = self.rpc_request(&[name]).await?;
        Ok(packages.pop())
    }

    pub async fn multi_info_of(&self, names: &[&str]) -> Result<Vec<Package>, Error> {
        self.rpc_request(names).await
    }

    pub async fn search_by_name(&self, query: &str) -> Result<Vec<Package>, Error> {
        let url = self.rpc_search_url(query, "name-desc")?;
        self.rpc_fetch(url).await
    }

    pub async fn resolve_dependencies(
        &self,
        dep_names: &[&str],
    ) -> Result<HashMap<String, DependencyResolution>, Error> {
        if dep_names.is_empty() {
            return Ok(HashMap::new());
        }

        let exact_aur_bases = self.resolve_bases(dep_names).await?;
        let mut resolutions = HashMap::new();
        let mut seen = HashSet::new();
        for dep_name in dep_names {
            if !seen.insert(*dep_name) {
                continue;
            }

            if self.local_repo_dependency_exists(dep_name)?
                || self.official_dependency_exists(dep_name).await?
            {
                resolutions.insert(dep_name.to_string(), DependencyResolution::Official);
                continue;
            }

            if let Some(pkgbase) = exact_aur_bases.get(*dep_name) {
                resolutions.insert(
                    dep_name.to_string(),
                    DependencyResolution::Aur {
                        pkgbase: pkgbase.clone(),
                    },
                );
                continue;
            }

            if let Some(pkgbase) = self.provider_pkgbase(dep_name).await? {
                resolutions.insert(dep_name.to_string(), DependencyResolution::Aur { pkgbase });
            }
        }

        Ok(resolutions)
    }

    fn rpc_search_url(&self, query: &str, by: &str) -> Result<Url, Error> {
        let mut url = Url::parse(&format!("{}/search", self.rpc_url))
            .map_err(|e| Error::Rpc(e.to_string()))?;
        url.path_segments_mut()
            .map_err(|_| Error::Rpc("Invalid RPC search URL".to_string()))?
            .push(query);
        url.query_pairs_mut().append_pair("by", by);
        Ok(url)
    }

    async fn rpc_fetch(&self, url: Url) -> Result<Vec<Package>, Error> {
        let http = self.http.clone();
        let fetch = move || {
            let http = http.clone();
            let url = url.clone();
            async move { http.get(url).send().await }
        };
        let resp = fetch
            .retry(
                FibonacciBuilder::default()
                    .with_min_delay(Duration::from_millis(500))
                    .with_max_times(3),
            )
            .await
            .map_err(Error::Http)?
            .error_for_status()
            .map_err(Error::Http)?;
        let text = resp.text().await?;
        let response: PackageResponse =
            serde_json::from_str(&text).map_err(|e| Error::Rpc(e.to_string()))?;
        if response.response_type == "error" {
            return Err(Error::Rpc(
                response
                    .error
                    .unwrap_or_else(|| "AUR RPC returned error".to_string()),
            ));
        }
        Ok(response.results)
    }

    async fn rpc_request(&self, args: &[&str]) -> Result<Vec<Package>, Error> {
        let packages = self.rpc_fetch(self.rpc_info_url(args)?).await?;
        if packages.is_empty() {
            return Err(Error::Rpc("package not found via RPC".into()));
        }
        Ok(packages)
    }

    /// Fetch package info and dependencies in a single RPC call.
    /// Returns (version, PkgDeps).
    pub async fn deps_of_with_version(&self, pkgbase: &str) -> Result<(String, PkgDeps), Error> {
        let packages = self.rpc_request(&[pkgbase]).await?;
        let pkg = packages
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotFound(pkgbase.to_string()))?;
        let version = pkg.version.clone();
        let deps = deps_from_packages(&[pkg]);
        Ok((version, deps))
    }

    async fn deps_of_rpc(&self, pkgbase: &str) -> Result<PkgDeps, Error> {
        let packages = self.rpc_request(&[pkgbase]).await?;
        Ok(deps_from_packages(&packages))
    }

    async fn official_dependency_exists(&self, dep_name: &str) -> Result<bool, Error> {
        if let Ok(found) = self.cached_official_dependency_exists(dep_name).await {
            return Ok(found);
        }
        let packages = self.official_search(dep_name).await?;
        Ok(packages.into_iter().any(|pkg| {
            pkg.pkgname == dep_name
                || pkg
                    .provides
                    .iter()
                    .any(|provide| parse_dep(provide).0 == dep_name)
        }))
    }

    async fn official_search(&self, query: &str) -> Result<Vec<OfficialPackage>, Error> {
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
                    .with_min_delay(Duration::from_millis(500))
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

    async fn provider_pkgbase(&self, dep_name: &str) -> Result<Option<String>, Error> {
        let mut packages = self
            .rpc_fetch(self.rpc_search_url(dep_name, "provides")?)
            .await?;
        packages.sort_by(|left, right| {
            left.package_base
                .cmp(&right.package_base)
                .then(left.name.cmp(&right.name))
        });
        Ok(packages.into_iter().next().map(|pkg| pkg.package_base))
    }

    async fn cached_official_dependency_exists(&self, dep_name: &str) -> Result<bool, Error> {
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

    fn local_repo_dependency_exists(&self, dep_name: &str) -> Result<bool, Error> {
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
}

/// Extract dependencies and sub-package names from a parsed .SRCINFO.
pub fn deps_from_srcinfo(source_info: &SourceInfoV1) -> PkgDeps {
    let packages = source_info
        .packages_for_architecture(alpm_types::SystemArchitecture::X86_64)
        .collect::<Vec<_>>();

    PkgDeps {
        depends: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.dependencies.iter().map(ToString::to_string)),
        ),
        make_depends: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.make_dependencies.iter().map(ToString::to_string)),
        ),
        pkgnames: collect_unique_strings(packages.iter().map(|pkg| pkg.name.to_string())),
        provides: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.provides.iter().map(ToString::to_string)),
        ),
    }
}

/// Split a pacman-style dependency string into `(name, version_constraint)`.
/// e.g. "glibc>=2.35" -> ("glibc", ">=2.35")
/// e.g. "python" -> ("python", "")
///
/// This parser only recognizes the standard pacman comparison operators
/// `>=`, `<=`, `=`, `>`, and `<`.
pub fn parse_dep(dep: &str) -> (&str, &str) {
    let dep = dep.trim();
    for &op in &[">=", "<=", "=", ">", "<"] {
        if let Some(pos) = dep.find(op) {
            let name = dep[..pos].trim();
            let constraint = dep[pos..].trim();
            return (name, constraint);
        }
    }
    (dep, "")
}

fn deps_from_packages(packages: &[Package]) -> PkgDeps {
    PkgDeps {
        depends: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.depends.iter().flatten().cloned()),
        ),
        make_depends: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.make_depends.iter().flatten().cloned()),
        ),
        pkgnames: collect_unique_strings(packages.iter().map(|pkg| pkg.name.clone())),
        provides: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.provides.iter().flatten().cloned()),
        ),
    }
}

fn collect_unique_strings<I>(values: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter_map(|value| seen.insert(value.clone()).then_some(value))
        .collect()
}

fn official_packages_url_for_aur(rpc_url: &str) -> String {
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

const OFFICIAL_REPO_NAMES: &[&str] = &["core", "extra", "multilib"];
const OFFICIAL_REPO_CACHE_TTL_SECS: u64 = 60 * 60;

fn default_repo_root() -> PathBuf {
    std::env::var("AURCACHE_REPO_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./repo"))
}

fn default_official_mirrorlist_path() -> PathBuf {
    if let Ok(path) = std::env::var("OFFICIAL_MIRRORLIST_PATH") {
        return PathBuf::from(path);
    }

    let base = std::env::var("MIRRORLIST_PATH_X86_64")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./config/pacman_x86_64"));
    base.join("mirrorlist")
}

fn default_official_repo_cache_dir() -> PathBuf {
    if let Ok(path) = std::env::var("OFFICIAL_REPO_CACHE_DIR") {
        return PathBuf::from(path);
    }

    default_official_mirrorlist_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("official_repo_cache")
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

fn cache_file_name(repo_name: &str) -> String {
    format!("{repo_name}.db.tar.gz")
}

#[cfg(test)]
mod tests {
    use super::{Package, deps_from_packages};

    #[test]
    fn deps_from_packages_collects_generic_dependencies() {
        let pkg: Package = serde_json::from_value(serde_json::json!({
            "Name": "parent",
            "Version": "1.0.0",
            "Description": null,
            "Maintainer": null,
            "URL": null,
            "NumVotes": 0,
            "Popularity": 0.0,
            "OutOfDate": null,
            "PackageBase": "parent",
            "PackageBaseID": 0,
            "FirstSubmitted": 0,
            "LastModified": 0,
            "URLPath": null,
            "ID": 0,
            "Depends": ["common-lib"],
            "MakeDepends": ["build-tool"],
            "OptDepends": null,
            "CheckDepends": null,
            "Conflicts": null,
            "Provides": null,
            "Replaces": null,
            "Groups": null,
            "License": null,
            "Keywords": null
        }))
        .unwrap();

        let deps = deps_from_packages(&[pkg]);

        assert_eq!(deps.depends, vec!["common-lib".to_string()]);
        assert_eq!(deps.make_depends, vec!["build-tool".to_string()]);
    }
}
