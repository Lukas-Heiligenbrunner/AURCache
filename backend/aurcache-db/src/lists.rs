//! The list columns, each a type that reads and writes its own encoding.
//!
//! Several columns are joined strings rather than richer types, and they do
//! not agree on the delimiter: the package columns use `;`, the worker columns
//! `,`, and some hold JSON. Each has a type here, so a value can only be read the way it was
//! written -- a dump once split the worker columns on `;`, and restored a
//! hand-edited worker as one unknown architecture -- and nothing else splits or
//! joins them.

use pacman_mirrors::platforms::Platform;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::str::FromStr;

/// The entries of a delimited string, trimmed, with empties dropped: an empty
/// column is no entries, not one empty entry, and a stray space is not part of
/// a name.
fn split(value: &str, delimiter: char) -> impl Iterator<Item = &str> {
    value
        .split(delimiter)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
}

/// `packages.platforms`: what a package is built for, `;`-joined, sorted and
/// without repeats -- so the same set always stores, and compares, the same
/// way whatever order it was asked for in.
#[derive(Clone, Debug, Default, PartialEq, Eq, sea_orm::DeriveValueType)]
#[sea_orm(value_type = "String")]
pub struct Platforms(Vec<Platform>);

impl Platforms {
    #[must_use]
    pub fn new(platforms: impl IntoIterator<Item = Platform>) -> Self {
        let mut platforms: Vec<Platform> = platforms.into_iter().collect();
        platforms.sort_by_key(Platform::as_str);
        platforms.dedup();
        Self(platforms)
    }

    #[must_use]
    pub fn as_slice(&self) -> &[Platform] {
        &self.0
    }

    #[must_use]
    pub fn contains(&self, platform: Platform) -> bool {
        self.0.contains(&platform)
    }

    /// The names, as the API sends them.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.0.iter().map(ToString::to_string).collect()
    }
}

impl fmt::Display for Platforms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.0.iter().map(Platform::as_str).collect();
        f.write_str(&names.join(";"))
    }
}

/// Unknown names are dropped, with a warning: names are checked where they
/// come in, so one here was written by a version that knew a platform this
/// one does not, and failing the row would make the package unreadable.
impl FromStr for Platforms {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(split(value, ';').filter_map(|name| {
            name.parse()
                .map_err(|e| tracing::warn!("ignoring a stored platform: {e}"))
                .ok()
        })))
    }
}

/// A list of names stored joined by `$joiner` and split on `$delimiter`, in
/// the order given, each trimmed and blanks dropped.
macro_rules! delimited_list {
    ($(#[$doc:meta])* $name:ident, $delimiter:literal, $joiner:literal) => {
        $(#[$doc])*
        #[derive(Clone, Debug, Default, PartialEq, Eq, sea_orm::DeriveValueType)]
        #[sea_orm(value_type = "String")]
        pub struct $name(Vec<String>);

        impl $name {
            #[must_use]
            pub fn new(entries: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
                Self(
                    entries
                        .into_iter()
                        .map(|entry| entry.as_ref().trim().to_string())
                        .filter(|entry| !entry.is_empty())
                        .collect(),
                )
            }

            #[must_use]
            pub fn as_slice(&self) -> &[String] {
                &self.0
            }

            #[must_use]
            pub fn into_vec(self) -> Vec<String> {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0.join($joiner))
            }
        }

        impl FromStr for $name {
            type Err = Infallible;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self::new(split(value, $delimiter)))
            }
        }
    };
}

delimited_list!(
    /// `packages.build_flags`: makepkg flags, `;`-joined.
    BuildFlags,
    ';',
    ";"
);

delimited_list!(
    /// The worker columns -- `arches`, `package_affinity` -- `,`-joined.
    WorkerList,
    ',',
    ","
);

delimited_list!(
    /// `packages.source_licenses`: what `.SRCINFO` lists, joined with `, ` as
    /// the package page shows it.
    Licenses,
    ',',
    ", "
);

/// A JSON-array column -- `packages.split_packages` and `packages.provides`.
///
/// Read best-effort: these columns are derived from a PKGBUILD and rewritten
/// on the next resolve, so a value that does not parse costs its names until
/// then, with a warning, rather than the whole row.
#[derive(Clone, Debug, Default, PartialEq, Eq, sea_orm::DeriveValueType)]
#[sea_orm(value_type = "String")]
pub struct JsonList(Vec<String>);

impl JsonList {
    #[must_use]
    pub fn new(entries: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(entries.into_iter().map(Into::into).collect())
    }

    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// The entries of a nullable column: none for `NULL`.
    #[must_use]
    pub fn entries(list: Option<&Self>) -> &[String] {
        list.map_or(&[], Self::as_slice)
    }
}

impl fmt::Display for JsonList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&serde_json::to_string(&self.0).map_err(|_| fmt::Error)?)
    }
}

impl FromStr for JsonList {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(serde_json::from_str(value).unwrap_or_else(|e| {
            tracing::warn!("ignoring an unreadable stored list: {e}");
            Vec::new()
        })))
    }
}

/// `builds.vcs_sources`: what a build's VCS sources were at, a JSON object of
/// `source_url -> commit`.
///
/// Read best-effort like [`JsonList`]: a stored value nobody can parse is not
/// evidence of anything, so it reads as nothing recorded, with a warning.
#[derive(Clone, Debug, Default, PartialEq, Eq, sea_orm::DeriveValueType)]
#[sea_orm(value_type = "String")]
pub struct VcsCommits(BTreeMap<String, String>);

impl VcsCommits {
    #[must_use]
    pub fn new(commits: BTreeMap<String, String>) -> Self {
        Self(commits)
    }

    #[must_use]
    pub fn into_map(self) -> BTreeMap<String, String> {
        self.0
    }
}

impl fmt::Display for VcsCommits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&serde_json::to_string(&self.0).map_err(|_| fmt::Error)?)
    }
}

impl FromStr for VcsCommits {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(serde_json::from_str(value).unwrap_or_else(|e| {
            tracing::warn!("ignoring unreadable recorded VCS commits: {e}");
            BTreeMap::new()
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Platform::{Aarch64, Armv7h, X86_64};

    #[test]
    fn an_empty_column_means_no_entries() {
        assert!("".parse::<BuildFlags>().unwrap().as_slice().is_empty());
        assert_eq!(
            "a;;b ".parse::<BuildFlags>().unwrap().as_slice(),
            ["a", "b"]
        );
        assert_eq!(
            "a, b ,c".parse::<WorkerList>().unwrap().as_slice(),
            ["a", "b", "c"]
        );
    }

    /// The JSON columns store exactly what `serde_json` writes, and read back
    /// unreadable as nothing.
    #[test]
    fn a_json_column_reads_back_as_it_was_written() {
        let list = JsonList::new(["a", "b"]);
        assert_eq!(list.to_string(), r#"["a","b"]"#);
        assert_eq!(list.to_string().parse::<JsonList>().unwrap(), list);
        assert!("nope".parse::<JsonList>().unwrap().as_slice().is_empty());

        let commits = VcsCommits::new(BTreeMap::from([("u".to_string(), "c".to_string())]));
        assert_eq!(commits.to_string(), r#"{"u":"c"}"#);
        assert_eq!(commits.to_string().parse::<VcsCommits>().unwrap(), commits);
        assert!("nope".parse::<VcsCommits>().unwrap().into_map().is_empty());
    }

    #[test]
    fn a_list_reads_back_as_it_was_written() {
        let flags = BuildFlags::new(["--noconfirm", "--nocolor"]);
        assert_eq!(flags.to_string(), "--noconfirm;--nocolor");
        assert_eq!(flags.to_string().parse::<BuildFlags>().unwrap(), flags);
        let arches = WorkerList::new(["x86_64", "aarch64"]);
        assert_eq!(arches.to_string(), "x86_64,aarch64");
        assert_eq!(arches.to_string().parse::<WorkerList>().unwrap(), arches);
    }

    /// The same set in any order -- or with repeats -- stores one way, so
    /// re-listing it never reads as a change, and a row written before that
    /// reads as the same set.
    #[test]
    fn platforms_are_stored_in_one_order() {
        assert_eq!(
            Platforms::new([Aarch64, X86_64]),
            Platforms::new([X86_64, Aarch64, X86_64])
        );
        assert_eq!(
            Platforms::new([Armv7h, Aarch64, X86_64, Aarch64]).to_string(),
            "aarch64;armv7h;x86_64"
        );
        assert_eq!(
            "x86_64;aarch64;x86_64".parse::<Platforms>().unwrap(),
            Platforms::new([X86_64, Aarch64])
        );
        assert_eq!(
            "x86_64;sparc".parse::<Platforms>().unwrap(),
            Platforms::new([X86_64]),
            "an unknown name is dropped, not the row"
        );
    }
}
