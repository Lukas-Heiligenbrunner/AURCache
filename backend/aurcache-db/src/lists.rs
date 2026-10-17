//! The list columns, each a type that reads and writes its own encoding.
//!
//! Several columns are joined strings rather than richer types, and they do
//! not agree on the delimiter: the package columns use `;`, the worker columns
//! `,`. Each has a type here, so a value can only be read the way it was
//! written -- a dump once split the worker columns on `;`, and restored a
//! hand-edited worker as one unknown architecture -- and nothing else splits or
//! joins them.

use pacman_mirrors::platforms::Platform;
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

/// Write `entries` joined by `delimiter`.
fn join<T: fmt::Display>(
    f: &mut fmt::Formatter<'_>,
    entries: &[T],
    delimiter: char,
) -> fmt::Result {
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            write!(f, "{delimiter}")?;
        }
        write!(f, "{entry}")?;
    }
    Ok(())
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
        join(f, &self.0, ';')
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

/// `packages.build_flags`: makepkg flags, `;`-joined, in the order given.
#[derive(Clone, Debug, Default, PartialEq, Eq, sea_orm::DeriveValueType)]
#[sea_orm(value_type = "String")]
pub struct BuildFlags(Vec<String>);

impl BuildFlags {
    /// The flags with surrounding whitespace trimmed and blank ones dropped.
    #[must_use]
    pub fn new(flags: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self(
            flags
                .into_iter()
                .map(|flag| flag.as_ref().trim().to_string())
                .filter(|flag| !flag.is_empty())
                .collect(),
        )
    }

    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

impl fmt::Display for BuildFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        join(f, &self.0, ';')
    }
}

impl FromStr for BuildFlags {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(split(value, ';')))
    }
}

/// The worker columns -- `native_arches`, `emulated_arches`,
/// `package_affinity` -- `,`-joined, in the order the worker gave them.
#[derive(Clone, Debug, Default, PartialEq, Eq, sea_orm::DeriveValueType)]
#[sea_orm(value_type = "String")]
pub struct WorkerList(Vec<String>);

impl WorkerList {
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

impl fmt::Display for WorkerList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        join(f, &self.0, ',')
    }
}

impl FromStr for WorkerList {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(split(value, ',')))
    }
}

/// The entries of a JSON-array column -- `packages.split_packages` and
/// `packages.provides` -- with nothing for an empty or unreadable one.
///
/// Best-effort like the readers it replaces: these columns are derived from a
/// PKGBUILD and rewritten on the next resolve, so a value that does not parse
/// costs a name until then rather than the whole request.
#[must_use]
pub fn json_list(value: Option<&str>) -> Vec<String> {
    value
        .and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or_default()
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
