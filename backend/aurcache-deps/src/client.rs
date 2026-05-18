use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use backon::{FibonacciBuilder, Retryable};
use reqwest::Client;
use url::Url;

use crate::deps::{deps_from_packages, parse_dep};
use crate::model::{DependencyResolution, Error, Package, PackageResponse, PkgDeps};
use crate::repo::{
    default_official_mirrorlist_path, default_official_repo_cache_dir,
    official_packages_url_for_aur,
};

#[derive(Debug, Clone)]
pub struct AurClient {
    pub(crate) http: Client,
    pub(crate) rpc_url: String,
    pub(crate) official_packages_url: String,
    pub(crate) repo_root: PathBuf,
    pub(crate) official_mirrorlist_path: PathBuf,
    pub(crate) official_repo_cache_dir: PathBuf,
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
            repo_root: crate::repo::default_repo_root(),
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
            repo_root: crate::repo::default_repo_root(),
            official_mirrorlist_path: default_official_mirrorlist_path(),
            official_repo_cache_dir: default_official_repo_cache_dir(),
            rpc_url,
        }
    }

    pub fn with_urls(aur_url: impl Into<String>, official_packages_url: impl Into<String>) -> Self {
        Self::with_urls_and_paths(
            aur_url,
            official_packages_url,
            crate::repo::default_repo_root(),
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

    pub(crate) fn official_search_url(&self, query: &str) -> Result<Url, Error> {
        let mut url =
            Url::parse(&self.official_packages_url).map_err(|e| Error::Rpc(e.to_string()))?;
        url.query_pairs_mut().append_pair("q", query);
        Ok(url)
    }

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

    pub(crate) async fn official_dependency_exists(&self, dep_name: &str) -> Result<bool, Error> {
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
}
