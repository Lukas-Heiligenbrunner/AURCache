use std::collections::HashMap;

use serde::Deserialize;
use thiserror::Error;

/// Errors that can occur when querying the AUR or resolving dependencies.
#[derive(Debug, Error)]
pub enum Error {
    /// The AUR RPC returned an error or an unexpected response.
    #[error("AUR RPC error: {0}")]
    Rpc(String),
    /// An underlying HTTP error from `reqwest`.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// Reading or writing the local repo / official-repo DB cache failed.
    #[error("repository cache I/O failed")]
    Io(#[from] std::io::Error),
    /// A repository or RPC URL could not be constructed.
    #[error("invalid URL")]
    Url(#[from] url::ParseError),
    /// The AUR RPC returned a body that is not the JSON we expect.
    #[error("malformed AUR RPC response")]
    Json(#[from] serde_json::Error),
    /// A cached repository database could not be read.
    #[error("malformed repository database")]
    RepoDb(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Nothing has managed to read the official repository databases yet.
    ///
    /// Its own variant so callers can say which of the sources is missing.
    /// Resolution cannot proceed without it -- a name the repositories hold
    /// would otherwise be sent to the AUR and built -- and the server serves
    /// while it retries, so this reaches a user as the reason an add failed.
    #[error(
        "the official repository databases have not been read yet; \
         check that a mirror is reachable"
    )]
    OfficialReposUnread,
}

/// Dependency lists extracted from a package's metadata.
#[derive(Debug, Clone)]
pub struct PkgDeps {
    /// Runtime dependencies.
    pub depends: Vec<String>,
    /// Build-time dependencies.
    pub make_depends: Vec<String>,
    /// All sub-package names provided by the pkgbase.
    pub pkgnames: Vec<String>,
    /// Virtual packages provided by this pkgbase.
    pub provides: Vec<String>,
}

/// Where a resolved dependency was found.
///
/// The three variants are three different *outcomes*, not three places:
/// nothing to do, link to something we track, or go build it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyResolution {
    /// Already installable as a binary, from an official Arch repository or
    /// from AURCache's own -- so there is nothing to build and nothing to
    /// record.
    ///
    /// Deliberately not called `Official`: a package AURCache built itself
    /// lands here too, whenever the database had no row to offer for it. The
    /// old name said "official repository" while the code also meant "our own
    /// repository", and the two disagreed about whether a dependency link
    /// should exist.
    Available,
    /// Satisfied by a package AURCache tracks. The only variant that can
    /// become a dependency edge, which is why it is the only one resolved
    /// against the database.
    Local { pkgbase: String },
    /// Not available anywhere yet; must be built from the AUR.
    Aur { pkgbase: String },
}

/// One dependency to resolve: the name, and the version it was declared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dependency<'a> {
    pub name: &'a str,
    /// A pacman constraint such as `">=2.0"`, or empty for an unversioned
    /// dependency.
    pub constraint: &'a str,
}

impl<'a> Dependency<'a> {
    #[must_use]
    pub fn new(name: &'a str, constraint: &'a str) -> Self {
        Self { name, constraint }
    }

    /// An unversioned dependency, for callers that have only a name.
    #[must_use]
    pub fn unversioned(name: &'a str) -> Self {
        Self::new(name, "")
    }
}

/// The outcome of resolving a set of dependencies.
#[derive(Debug, Default, Clone)]
pub struct Resolutions {
    /// Where each dependency was found.
    pub found: HashMap<String, DependencyResolution>,
    /// Names that matched nothing: no repository holds them, no AUR package
    /// carries the name, and nothing declares them in `provides`.
    ///
    /// Reported rather than silently dropped. A typo, or a dependency removed
    /// from the AUR, used to vanish during resolution and resurface much later
    /// as an opaque `makepkg` failure with nothing pointing back to here.
    pub unresolved: Vec<String>,
}

impl Resolutions {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&DependencyResolution> {
        self.found.get(name)
    }
}

/// Package metadata returned by the AUR RPC v5 `/info` or `/search` endpoints.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[serde(default)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub maintainer: Option<String>,
    #[serde(rename = "URL")]
    pub url: Option<String>,
    pub num_votes: u32,
    pub popularity: f32,
    pub out_of_date: Option<u32>,
    pub package_base: String,
    #[serde(rename = "PackageBaseID")]
    pub package_base_id: u32,
    pub first_submitted: u32,
    pub last_modified: u32,
    #[serde(rename = "URLPath")]
    pub url_path: Option<String>,
    #[serde(rename = "ID")]
    pub id: u32,
    pub depends: Option<Vec<String>>,
    pub make_depends: Option<Vec<String>>,
    pub opt_depends: Option<Vec<String>>,
    pub check_depends: Option<Vec<String>>,
    pub conflicts: Option<Vec<String>>,
    pub provides: Option<Vec<String>>,
    pub replaces: Option<Vec<String>>,
    pub groups: Option<Vec<String>>,
    pub license: Option<Vec<String>>,
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
