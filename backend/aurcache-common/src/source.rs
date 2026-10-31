//! How a package's source is identified.
//!
//! Shared rather than defined beside the database entity: these shapes appear
//! in API requests and responses, so they have to be usable from a frontend
//! that has no database driver.

use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::str::FromStr;
use utoipa::ToSchema;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GitSourceSpec {
    pub url: String,
    pub r#ref: String,
    pub subfolder: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
// Stored in one database column as its `Display` form. The derive that makes
// that work is a database concern, so it is gated: a frontend gets the same
// shape without a database driver.
#[cfg_attr(feature = "db", derive(sea_orm::DeriveValueType))]
#[cfg_attr(feature = "db", sea_orm(value_type = "String"))]
#[serde(tag = "type")]
pub enum SourceData {
    #[serde(rename = "aur")]
    Aur { name: String },
    #[serde(rename = "git")]
    Git {
        #[serde(flatten)]
        spec: GitSourceSpec,
    },
    #[serde(rename = "upload")]
    Upload { archive: Vec<u8> },
}

/// Whether a package someone typed is a git remote rather than an AUR name.
///
/// Git URLs either use the SCP-like `user@host:path` shorthand — any user, not
/// just `git`, e.g. `aur@aur.archlinux.org:foo.git` — or an explicit scheme
/// (`https://`, `ssh://`, `git://`, …). An AUR package name cannot contain `@`,
/// so anything with one is unambiguously a remote.
///
/// A bare `.git` suffix is deliberately not enough: AUR names legitimately end
/// in it (`paru-git`, `lab.git`), and with no scheme there is no filesystem to
/// resolve a scheme-less path against anyway. Those stay AUR names.
///
/// Shared because the CLI and the web UI both accept one field that is either
/// thing, and two copies of a guess like this would drift into disagreeing
/// about the same input.
#[must_use]
pub fn looks_like_git_url(s: &str) -> bool {
    s.contains('@') || s.contains("://")
}

/// How to write a source on screen: the AUR name, the git remote with its
/// ref and subfolder, or a placeholder for an upload.
///
/// The label is derived rather than stored: two sources with the same label
/// are the same source, which is what duplicate checks rely on — and a
/// failure is reported against what the request carried.
///
/// Shared for the same reason as [`looks_like_git_url`]: the add dialog, the
/// progress cards and the server's bulk-add report each had a copy, with only
/// punctuation drift between them.
#[must_use]
pub fn source_label(source: &SourceData) -> String {
    match source {
        SourceData::Aur { name } => name.clone(),
        SourceData::Git { spec } => {
            let mut label = spec.url.clone();
            if !spec.r#ref.is_empty() {
                label.push('#');
                label.push_str(&spec.r#ref);
            }
            if !spec.subfolder.is_empty() {
                label.push('/');
                label.push_str(&spec.subfolder);
            }
            label
        }
        // Never built from here — the upload it belongs to was never
        // implemented server-side — but the variant exists, so it gets a
        // label rather than a panic.
        SourceData::Upload { .. } => "uploaded archive".to_string(),
    }
}

impl From<GitSourceSpec> for SourceData {
    fn from(spec: GitSourceSpec) -> Self {
        Self::Git { spec }
    }
}

impl FromStr for SourceData {
    /// The error this actually produces, rather than `anyhow::Error` wrapping
    /// it. sea-orm 2.0 requires a `std::error::Error` here, and naming the real
    /// one is better than boxing it: `?` still converts it into `anyhow` at
    /// every call site that wants that.
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl SourceData {
    /// Unique cache key for this source.
    ///
    /// The git fields are length-prefixed, not plain-joined: they are URLs
    /// and refs, which contain `:`, so `url="a:b", ref="c"` and `url="a",
    /// ref="b:c"` would otherwise share a key — and check out over each
    /// other in the snapshot cache. Digits survive `sanitize_cache_key`,
    /// keeping the on-disk name unambiguous too.
    #[must_use]
    pub fn cache_key(&self) -> String {
        match self {
            Self::Aur { name } => format!("aur:{name}"),
            Self::Git { spec } => {
                format!(
                    "git:{}:{}:{}:{}:{}:{}",
                    spec.url.len(),
                    spec.url,
                    spec.r#ref.len(),
                    spec.r#ref,
                    spec.subfolder.len(),
                    spec.subfolder
                )
            }
            Self::Upload { .. } => String::from("upload"),
        }
    }
}

impl Display for SourceData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&serde_json::to_string(self).map_err(|_| std::fmt::Error)?)
    }
}

#[cfg(test)]
mod tests {
    use super::{SourceData, looks_like_git_url};

    #[test]
    fn detects_scp_like_git_urls() {
        assert!(looks_like_git_url("aur@aur.archlinux.org:paru"));
        assert!(looks_like_git_url("git@github.com:user/project"));
    }

    #[test]
    fn detects_scheme_git_urls() {
        assert!(looks_like_git_url("https://github.com/user/project"));
        assert!(looks_like_git_url("ssh://git@host/repo"));
        assert!(looks_like_git_url("git://host/repo"));
    }

    /// The suffix alone is not a URL: these are real AUR package names, and
    /// treating them as remotes would make them unaddable by name.
    #[test]
    fn does_not_treat_git_like_aur_names_as_urls() {
        assert!(!looks_like_git_url("paru-git"));
        assert!(!looks_like_git_url("lab.git"));
        assert!(!looks_like_git_url("hello"));
    }

    /// Colons move across field boundaries: `url="a:b", ref="c"` and
    /// `url="a", ref="b:c"` must not share a snapshot cache directory.
    #[test]
    fn git_cache_keys_survive_colons_in_every_field() {
        use crate::source::GitSourceSpec;
        let left = SourceData::Git {
            spec: GitSourceSpec {
                url: "a:b".to_string(),
                r#ref: "c".to_string(),
                subfolder: String::new(),
            },
        };
        let right = SourceData::Git {
            spec: GitSourceSpec {
                url: "a".to_string(),
                r#ref: "b:c".to_string(),
                subfolder: String::new(),
            },
        };
        assert_ne!(left.cache_key(), right.cache_key());
    }
}
