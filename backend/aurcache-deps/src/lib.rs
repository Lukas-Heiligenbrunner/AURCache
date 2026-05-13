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
    aur_url: String,
}

impl AurClient {
    pub fn new() -> Self {
        let aur_url = std::env::var("AUR_BASE_URL")
            .unwrap_or_else(|_| "https://aur.archlinux.org".to_string());
        Self {
            http: Client::new(),
            aur_url,
        }
    }

    pub fn with_aur_url(aur_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            aur_url: aur_url.into(),
        }
    }

    fn snapshot_url(&self, pkgbase: &str) -> String {
        format!("{}/cgit/aur.git/snapshot/{}.tar.gz", self.aur_url, pkgbase)
    }

    fn rpc_info_url(&self, args: &[&str]) -> Result<Url, Error> {
        let mut url = Url::parse(&format!("{}/rpc/v5/info", self.aur_url))
            .map_err(|e| Error::Rpc(e.to_string()))?;
        for arg in args {
            url.query_pairs_mut().append_pair("arg[]", arg);
        }
        Ok(url)
    }

    async fn fetch_snapshot_bytes(&self, pkgbase: &str) -> Result<Vec<u8>, Error> {
        let response = self.http.get(&self.snapshot_url(pkgbase)).send().await?;
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

#[derive(Deserialize)]
struct PackageInfo {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "PackageBase")]
    package_base: String,
    #[serde(rename = "Depends", default)]
    depends: Vec<String>,
    #[serde(rename = "MakeDepends", default)]
    make_depends: Vec<String>,
}

#[derive(Deserialize)]
struct InfoResponse {
    #[serde(rename = "type")]
    response_type: String,
    results: Vec<PackageInfo>,
}

impl AurClient {
    pub async fn resolve_bases(&self, names: &[&str]) -> Result<HashMap<String, String>, Error> {
        let response: InfoResponse = self.rpc_request(names).await?;
        Ok(response
            .results
            .into_iter()
            .map(|pkg| (pkg.name, pkg.package_base))
            .collect())
    }

    pub async fn deps_of(&self, pkgbase: &str) -> Result<PkgDeps, Error> {
        match self.deps_of_rpc(pkgbase).await {
            Ok(deps) => return Ok(deps),
            Err(_) => {}
        }
        self.deps_of_srcinfo(pkgbase).await
    }

    async fn rpc_request(&self, args: &[&str]) -> Result<InfoResponse, Error> {
        let url = self.rpc_info_url(args)?;
        let resp = self.http.get(url).send().await?;
        let text = resp.text().await?;
        let response: InfoResponse =
            serde_json::from_str(&text).map_err(|e| Error::Rpc(e.to_string()))?;

        if response.response_type == "error" || response.results.is_empty() {
            return Err(Error::Rpc("package not found via RPC".into()));
        }

        Ok(response)
    }

    async fn deps_of_rpc(&self, pkgbase: &str) -> Result<PkgDeps, Error> {
        let response = self.rpc_request(&[pkgbase]).await?;
        Ok(deps_from_packages(&response.results))
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
            dedup_push(&mut pkgnames, &pkg.name.to_string());
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

fn dedup_push(vec: &mut Vec<String>, item: &str) {
    if !vec.iter().any(|x| x == item) {
        vec.push(item.to_string());
    }
}

fn deps_from_packages(packages: &[PackageInfo]) -> PkgDeps {
    let mut depends = Vec::new();
    let mut make_depends = Vec::new();
    let mut pkgnames = Vec::new();

    for pkg in packages {
        dedup_push(&mut pkgnames, &pkg.name);
        for d in &pkg.depends {
            dedup_push(&mut depends, d);
        }
        for d in &pkg.make_depends {
            dedup_push(&mut make_depends, d);
        }
    }

    PkgDeps {
        depends,
        make_depends,
        pkgnames,
    }
}
