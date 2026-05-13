use std::collections::HashMap;
use std::io::Read;

use alpm_srcinfo::SourceInfoV1;
use alpm_types::SystemArchitecture;
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
    #[error(".SRCINFO not found in snapshot for {0}")]
    MissingSrcinfo(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Srcinfo(#[from] alpm_srcinfo::Error),
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

impl AurClient {
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

    fn snapshot_url(&self, pkgbase: &str) -> String {
        let base = self
            .rpc_url
            .trim_end_matches("/rpc/v5")
            .trim_end_matches('/');
        format!("{base}/cgit/aur.git/snapshot/{pkgbase}.tar.gz")
    }

    fn rpc_info_url(&self, args: &[&str]) -> Result<Url, Error> {
        let mut url =
            Url::parse(&format!("{}/info", self.rpc_url)).map_err(|e| Error::Rpc(e.to_string()))?;
        for arg in args {
            url.query_pairs_mut().append_pair("arg[]", arg);
        }
        Ok(url)
    }

    async fn fetch_snapshot_bytes(&self, pkgbase: &str) -> Result<Vec<u8>, Error> {
        let response = self.http.get(self.snapshot_url(pkgbase)).send().await?;
        if !response.status().is_success() {
            return Err(Error::NotFound(pkgbase.into()));
        }
        Ok(response.bytes().await?.to_vec())
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
        let packages = self.rpc_request(names).await?;
        Ok(packages
            .into_iter()
            .map(|pkg| (pkg.name, pkg.package_base))
            .collect())
    }

    pub async fn deps_of(&self, pkgbase: &str) -> Result<PkgDeps, Error> {
        if let Ok(deps) = self.deps_of_rpc(pkgbase).await {
            return Ok(deps);
        }
        self.deps_of_srcinfo(pkgbase).await
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
        let resp = self.http.get(url).send().await?;
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

    async fn deps_of_rpc(&self, pkgbase: &str) -> Result<PkgDeps, Error> {
        let packages = self.rpc_request(&[pkgbase]).await?;
        Ok(deps_from_packages(&packages))
    }

    async fn deps_of_srcinfo(&self, pkgbase: &str) -> Result<PkgDeps, Error> {
        let bytes = self.fetch_snapshot_bytes(pkgbase).await?;

        let decoder = flate2::read::GzDecoder::new(&bytes[..]);
        let mut archive = tar::Archive::new(decoder);

        let mut srcinfo_content = None;
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?;
            if path.ends_with(".SRCINFO") {
                let mut content = String::new();
                entry.read_to_string(&mut content)?;
                srcinfo_content = Some(content);
                break;
            }
        }

        let content = srcinfo_content.ok_or_else(|| Error::MissingSrcinfo(pkgbase.to_owned()))?;
        let source_info = SourceInfoV1::from_string(&content)?;

        let mut depends = Vec::new();
        let mut make_depends = Vec::new();
        let mut pkgnames = Vec::new();

        for pkg in source_info.packages_for_architecture(SystemArchitecture::X86_64) {
            dedup_push(&mut pkgnames, pkg.name.as_ref());
            for dep in &pkg.dependencies {
                dedup_push(&mut depends, &dep.to_string());
            }
            for dep in &pkg.make_dependencies {
                dedup_push(&mut make_depends, &dep.to_string());
            }
        }

        Ok(PkgDeps {
            depends,
            make_depends,
            pkgnames,
        })
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

fn dedup_push(vec: &mut Vec<String>, item: &str) {
    if !vec.iter().any(|x| x == item) {
        vec.push(item.to_string());
    }
}

fn deps_from_packages(packages: &[Package]) -> PkgDeps {
    let mut depends = Vec::new();
    let mut make_depends = Vec::new();
    let mut pkgnames = Vec::new();

    for pkg in packages {
        dedup_push(&mut pkgnames, &pkg.name);
        if let Some(deps) = &pkg.depends {
            for d in deps {
                dedup_push(&mut depends, d);
            }
        }
        if let Some(deps) = &pkg.make_depends {
            for d in deps {
                dedup_push(&mut make_depends, d);
            }
        }
    }

    PkgDeps {
        depends,
        make_depends,
        pkgnames,
    }
}
