use serde::Deserialize;
use thiserror::Error;

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
pub(crate) struct PackageResponse {
    #[serde(rename = "type")]
    pub(crate) response_type: String,
    #[serde(default)]
    pub(crate) error: Option<String>,
    pub(crate) results: Vec<Package>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OfficialPackage {
    pub(crate) pkgname: String,
    #[serde(default)]
    pub(crate) provides: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OfficialPackageResponse {
    pub(crate) results: Vec<OfficialPackage>,
}
