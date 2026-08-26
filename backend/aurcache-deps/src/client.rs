use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use backon::{FibonacciBuilder, Retryable};
use reqwest::Client;
use url::Url;

/// Ceiling for a generated RPC URL, under the server's 8 KiB request-line
/// limit with room for the scheme, host and path already in the base.
const MAX_RPC_URL_BYTES: usize = 7_600;

use crate::deps::deps_from_packages;
use crate::model::{DependencyResolution, Error, Package, PackageResponse, PkgDeps};
use crate::repo::{default_official_mirrorlist_path, default_official_repo_cache_dir};

/// Client for the AUR RPC and Arch Linux official package search APIs.
///
/// Handles dependency resolution against AUR packages, official Arch
/// repositories (via a cached local copy of the repo DBs), and packages
/// already present in the local AURCache repository.
#[derive(Debug, Clone)]
pub struct AurClient {
    pub(crate) http: Client,
    pub(crate) rpc_url: String,
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
pub(crate) fn snapshot_url(rpc_url: &str, pkgbase: &str) -> String {
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
            repo_root: crate::repo::default_repo_root(),
            official_mirrorlist_path: default_official_mirrorlist_path(),
            official_repo_cache_dir: default_official_repo_cache_dir(),
            rpc_url,
        }
    }

    /// Construct a client with an explicit AUR RPC URL and default filesystem paths.
    pub fn with_urls(aur_url: impl Into<String>) -> Self {
        Self::with_urls_and_paths(
            aur_url,
            crate::repo::default_repo_root(),
            default_official_mirrorlist_path(),
            default_official_repo_cache_dir(),
        )
    }

    /// Construct a client with full control over the AUR RPC URL and filesystem paths.
    pub fn with_urls_and_paths(
        aur_url: impl Into<String>,
        repo_root: impl Into<PathBuf>,
        official_mirrorlist_path: impl Into<PathBuf>,
        official_repo_cache_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            http: Client::new(),
            rpc_url: aur_url.into(),
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

    /// Split an `/info` query across as many URLs as the length limit requires.
    ///
    /// The RPC takes one `arg[]` per package on a GET, and the server rejects a
    /// request line over 8 KiB with `414 Request-URI Too Large` — measured:
    /// 8185 bytes answers, 8313 does not. At roughly 25 bytes per package that
    /// caps a single request near 300 packages, which a repository of any size
    /// passes.
    ///
    /// Getting this wrong is not a partial failure: the version-check pass
    /// propagates the error, so one oversized request stops *every* package
    /// from being checked, on every pass, until the package count drops.
    ///
    /// Measured against the encoded URL rather than a package count, because
    /// names are percent-encoded — `aewm++` is four bytes longer than it looks.
    fn rpc_info_urls(&self, args: &[&str]) -> Result<Vec<Url>, Error> {
        let base = format!("{}/info", self.rpc_url);
        let mut urls = Vec::new();
        let mut current = Url::parse(&base).map_err(|e| Error::Rpc(e.to_string()))?;
        let mut in_current = 0usize;

        for arg in args {
            let mut candidate = current.clone();
            candidate.query_pairs_mut().append_pair("arg[]", arg);

            if in_current > 0 && candidate.as_str().len() > MAX_RPC_URL_BYTES {
                urls.push(current);
                current = Url::parse(&base).map_err(|e| Error::Rpc(e.to_string()))?;
                current.query_pairs_mut().append_pair("arg[]", arg);
                in_current = 1;
            } else {
                // A single argument that alone exceeds the budget still goes
                // out on its own: a doomed request beats silently dropping the
                // package from version checking.
                current = candidate;
                in_current += 1;
            }
        }

        if in_current > 0 {
            urls.push(current);
        }
        Ok(urls)
    }

    /// Resolve a list of package names to their pkgbase names via the AUR RPC.
    pub async fn resolve_bases(&self, names: &[&str]) -> Result<HashMap<String, String>, Error> {
        // Chunked for the same reason as `multi_info_of`: this is handed a
        // whole dependency list, which for a large package can outgrow the
        // server's URL limit.
        let mut packages = Vec::new();
        for url in self.rpc_info_urls(names)? {
            packages.extend(self.rpc_fetch(url).await?);
        }
        Ok(packages
            .into_iter()
            .map(|pkg| (pkg.name, pkg.package_base))
            .collect())
    }

    /// Fetch the dependency lists for a pkgbase via the AUR RPC.
    pub async fn deps_of(&self, pkgbase: &str) -> Result<PkgDeps, Error> {
        let packages = self.rpc_request(&[pkgbase]).await?;
        Ok(deps_from_packages(&packages))
    }

    /// Fetch metadata for a single AUR package by name, returning `None` if not found.
    pub async fn info_of(&self, name: &str) -> Result<Option<Package>, Error> {
        let packages = self.rpc_fetch(self.rpc_info_url(&[name])?).await?;
        Ok(packages.into_iter().next())
    }

    /// Fetch metadata for multiple AUR packages in a single RPC call.
    pub async fn multi_info_of(&self, names: &[&str]) -> Result<Vec<Package>, Error> {
        let mut packages = Vec::new();
        for url in self.rpc_info_urls(names)? {
            // An individual chunk returning nothing is fine — those packages
            // are simply not in the AUR any more. Only a request that fails
            // outright aborts.
            packages.extend(self.rpc_fetch(url).await?);
        }

        if packages.is_empty() && !names.is_empty() {
            return Err(Error::Rpc("package not found via RPC".into()));
        }
        Ok(packages)
    }

    /// Search AUR packages by name or description.
    pub async fn search_by_name(&self, query: &str) -> Result<Vec<Package>, Error> {
        let url = self.rpc_search_url(query, "name-desc")?;
        self.rpc_fetch(url).await
    }

    /// Resolve a list of dependency names to their sources (AUR, official, or local repo).
    ///
    /// Dependencies not found anywhere are omitted from the result map.
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
        let resp = self.retry_get(url).await?;
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

    /// Perform an HTTP GET with the shared Fibonacci retry policy, returning the
    /// response only if it has a success status.
    pub(crate) async fn retry_get<U: reqwest::IntoUrl + Clone>(
        &self,
        url: U,
    ) -> Result<reqwest::Response, Error> {
        let http = self.http.clone();
        let fetch = move || {
            let http = http.clone();
            let url = url.clone();
            async move { http.get(url).send().await }
        };
        fetch
            .retry(
                FibonacciBuilder::default()
                    .with_min_delay(Duration::from_millis(500))
                    .with_max_times(3),
            )
            .await
            .map_err(Error::Http)?
            .error_for_status()
            .map_err(Error::Http)
    }

    /// Download the raw snapshot tarball for an AUR pkgbase.
    pub async fn download_snapshot_bytes(&self, pkgbase: &str) -> Result<Vec<u8>, Error> {
        let url = snapshot_url(&self.rpc_url, pkgbase);
        let resp = self.retry_get(url).await?;
        let bytes = resp.bytes().await?.to_vec();
        Ok(bytes)
    }

    pub(crate) async fn official_dependency_exists(&self, dep_name: &str) -> Result<bool, Error> {
        Ok(self
            .cached_official_dependency_exists(dep_name)
            .await
            .unwrap_or(false))
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

#[cfg(test)]
mod url_chunking_tests {
    use super::{AurClient, MAX_RPC_URL_BYTES};

    fn client() -> AurClient {
        AurClient::with_urls("https://aur.archlinux.org/rpc/v5")
    }

    /// A handful of packages is one request, as before.
    #[test]
    fn a_small_query_is_a_single_request() {
        let names = ["hello", "yay", "paru"];
        let urls = client().rpc_info_urls(&names).expect("urls");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].query_pairs().count(), 3);
    }

    /// The case that broke: a repository with enough packages to outgrow the
    /// server's 8 KiB request line. Measured against the real service — 8185
    /// bytes answers, 8313 returns 414.
    #[test]
    fn a_large_query_is_split_and_every_part_fits() {
        let names: Vec<String> = (0..1000)
            .map(|i| format!("some-package-name-{i}"))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();

        let urls = client().rpc_info_urls(&refs).expect("urls");
        assert!(urls.len() > 1, "1000 packages should not be one request");

        for url in &urls {
            assert!(
                url.as_str().len() <= MAX_RPC_URL_BYTES,
                "chunk of {} bytes exceeds the budget",
                url.as_str().len()
            );
        }

        // Every package is asked about exactly once — chunking must not drop
        // or duplicate any, which would silently stop them being checked.
        let asked: Vec<String> = urls
            .iter()
            .flat_map(|u| {
                u.query_pairs()
                    .map(|(_, v)| v.to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(asked, names);
    }

    /// Names are percent-encoded, so a count-based split would misjudge the
    /// length. 187 AUR packages contain `+`, which triples in the URL.
    #[test]
    fn encoded_names_are_measured_at_their_encoded_length() {
        let names: Vec<String> = (0..1000).map(|i| format!("aewm++{i}+plus+name")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();

        for url in client().rpc_info_urls(&refs).expect("urls") {
            assert!(
                url.as_str().len() <= MAX_RPC_URL_BYTES,
                "encoded chunk of {} bytes exceeds the budget",
                url.as_str().len()
            );
        }
    }

    /// No packages means no requests, rather than one pointless empty query.
    #[test]
    fn an_empty_query_makes_no_requests() {
        assert!(client().rpc_info_urls(&[]).expect("urls").is_empty());
    }
}
