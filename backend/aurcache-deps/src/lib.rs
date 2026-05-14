use std::collections::{HashMap, HashSet};
use std::time::Duration;

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
            rpc_url,
        }
    }

    pub fn with_aur_url(aur_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            rpc_url: aur_url.into(),
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
}

#[derive(Debug, Clone)]
pub struct PkgDeps {
    pub depends: Vec<String>,
    pub make_depends: Vec<String>,
    pub pkgnames: Vec<String>,
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
    results: Vec<Package>,
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
        let url = self.rpc_search_url(query)?;
        self.rpc_fetch(url).await
    }

    fn rpc_search_url(&self, query: &str) -> Result<Url, Error> {
        let mut url = Url::parse(&format!("{}/search", self.rpc_url))
            .map_err(|e| Error::Rpc(e.to_string()))?;
        url.query_pairs_mut().append_pair("by", "name-desc");
        url.query_pairs_mut().append_pair("arg[]", query);
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
            .map_err(Error::Http)?;
        let text = resp.text().await?;
        let response: PackageResponse =
            serde_json::from_str(&text).map_err(|e| Error::Rpc(e.to_string()))?;
        if response.response_type == "error" {
            return Err(Error::Rpc("AUR RPC returned error".into()));
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
}

/// Extract dependencies and sub-package names from a parsed .SRCINFO.
pub fn deps_from_srcinfo(source_info: &SourceInfoV1) -> PkgDeps {
    let mut depends = Vec::new();
    let mut make_depends = Vec::new();
    let mut pkgnames = Vec::new();
    let mut seen_depends = HashSet::new();
    let mut seen_make_depends = HashSet::new();
    let mut seen_pkgnames = HashSet::new();

    for pkg in source_info.packages_for_architecture(alpm_types::SystemArchitecture::X86_64) {
        if seen_pkgnames.insert(pkg.name.to_string()) {
            pkgnames.push(pkg.name.to_string());
        }
        for dep in &pkg.dependencies {
            let s = dep.to_string();
            if seen_depends.insert(s.clone()) {
                depends.push(s);
            }
        }
        for dep in &pkg.make_dependencies {
            let s = dep.to_string();
            if seen_make_depends.insert(s.clone()) {
                make_depends.push(s);
            }
        }
    }

    PkgDeps {
        depends,
        make_depends,
        pkgnames,
    }
}

/// Split a dependency string into (name, version_constraint).
/// e.g. "glibc>=2.35" -> ("glibc", ">=2.35")
/// e.g. "python" -> ("python", "")
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
    let mut depends = Vec::new();
    let mut make_depends = Vec::new();
    let mut pkgnames = Vec::new();
    let mut seen_depends = HashSet::new();
    let mut seen_make_depends = HashSet::new();
    let mut seen_pkgnames = HashSet::new();

    for pkg in packages {
        if seen_pkgnames.insert(pkg.name.clone()) {
            pkgnames.push(pkg.name.clone());
        }
        if let Some(deps) = &pkg.depends {
            for d in deps {
                if seen_depends.insert(d.clone()) {
                    depends.push(d.clone());
                }
            }
        }
        if let Some(deps) = &pkg.make_depends {
            for d in deps {
                if seen_make_depends.insert(d.clone()) {
                    make_depends.push(d.clone());
                }
            }
        }
    }

    PkgDeps {
        depends,
        make_depends,
        pkgnames,
    }
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
