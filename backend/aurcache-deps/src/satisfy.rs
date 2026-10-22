//! Which package satisfies a dependency name.
//!
//! Used for the packages AURCache tracks: the caller declares what each one
//! answers to, and [`SatisfyIndex::best_match`] is the only function that
//! decides which candidate wins -- an exact name above a split package above a
//! `provides` entry. (The official repositories only ever answer yes or no,
//! through `OfficialRepos::holds`; an AUR `provides` search ranks with
//! `provider_rank`.)

use std::collections::{HashMap, HashSet};

use crate::version::satisfies_constraint;

/// How directly a candidate provides a name. Ordered: a package answering to
/// its own name beats one of its split packages, which beats a `provides`
/// entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchKind {
    /// The pkgbase's own name.
    Name,
    /// One of the pkgbase's split package names.
    Split,
    /// Declared in the package's `provides`.
    Provides,
}

/// One candidate's claim on one dependency name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The package base to attribute the match to -- the only thing a caller
    /// can turn into a dependency edge or a build.
    pub pkgbase: String,
    pub kind: MatchKind,
    /// The version at which the name is provided, when the source records one:
    /// the package's own version for a [`MatchKind::Name`] or
    /// [`MatchKind::Split`] match, and the right-hand side of a versioned
    /// `provides` entry otherwise.
    ///
    /// `None` where the source has no version to offer. That is the norm for
    /// tracked packages, which are matched before anything has been built, and
    /// it is also what a bare (unversioned) `provides` entry gives.
    pub version: Option<String>,
}

impl Match {
    /// Whether this match's recorded version satisfies `constraint`.
    ///
    /// A match with no recorded version answers `true` only for an
    /// unversioned dependency: pacman treats a bare `provides` the same way,
    /// and a source that cannot say which version it holds cannot honestly
    /// claim to meet a bound.
    #[must_use]
    pub fn satisfies(&self, constraint: &str) -> bool {
        match &self.version {
            Some(version) => satisfies_constraint(version, constraint),
            None => constraint.trim().is_empty(),
        }
    }
}

/// Dependency names mapped to everything that claims to provide them.
#[derive(Debug, Default, Clone)]
pub struct SatisfyIndex {
    by_name: HashMap<String, Vec<Match>>,
}

impl SatisfyIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `pkgbase` provides `name`.
    ///
    /// Callers add every name a package answers to; nothing is deduplicated
    /// across sources, because [`Self::best_match`] ranks them anyway.
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        pkgbase: impl Into<String>,
        kind: MatchKind,
        version: Option<String>,
    ) {
        self.by_name.entry(name.into()).or_default().push(Match {
            pkgbase: pkgbase.into(),
            kind,
            version,
        });
    }

    /// Record everything one package claims: the name it is published under,
    /// and each entry in its `provides`.
    ///
    /// Only names in `wanted` are kept, which keeps the index proportional to
    /// the dependency list rather than to everything tracked.
    ///
    /// `pkgbase` is what a match is attributed to, since a dependency
    /// ultimately resolves to a package base. A `name` differing from it is
    /// one of a split package's outputs -- exactly [`MatchKind::Split`].
    pub fn insert_package(
        &mut self,
        name: &str,
        pkgbase: &str,
        version: Option<&str>,
        provides: impl IntoIterator<Item = impl AsRef<str>>,
        wanted: &HashSet<&str>,
    ) {
        if wanted.contains(name) {
            let kind = if name == pkgbase {
                MatchKind::Name
            } else {
                MatchKind::Split
            };
            self.insert(name, pkgbase, kind, version.map(ToString::to_string));
        }

        for provide in provides {
            // A `provides` entry is either a bare name or `name=version`;
            // unlike a dependency it never carries an inequality, so splitting
            // on `=` is the whole grammar.
            let provide = provide.as_ref();
            let (provided, provided_version) = match provide.split_once('=') {
                Some((provided, version)) => (provided.trim(), Some(version.trim().to_string())),
                None => (provide.trim(), None),
            };
            if wanted.contains(provided) {
                self.insert(provided, pkgbase, MatchKind::Provides, provided_version);
            }
        }
    }

    /// The best candidate for `name` among those `accept` allows.
    ///
    /// Ranked by [`MatchKind`] first, then by pkgbase so that two equally
    /// direct candidates resolve the same way on every run -- a `HashMap`'s
    /// iteration order would otherwise make an add non-deterministic.
    ///
    /// `accept` is where a caller applies its own extra condition, which in
    /// practice means the version constraint. Whether to apply one is a real
    /// difference between sources rather than an oversight, so it is the
    /// caller's to state: see [`crate::client::AurClient::resolve_dependencies`].
    pub fn best_match(&self, name: &str, accept: impl Fn(&Match) -> bool) -> Option<&Match> {
        self.by_name
            .get(name)?
            .iter()
            .filter(|candidate| accept(candidate))
            .min_by(|left, right| {
                left.kind
                    .cmp(&right.kind)
                    .then_with(|| left.pkgbase.cmp(&right.pkgbase))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{MatchKind, SatisfyIndex};

    fn index() -> SatisfyIndex {
        let mut index = SatisfyIndex::new();
        index.insert("thing", "provider-b", MatchKind::Provides, None);
        index.insert("thing", "provider-a", MatchKind::Provides, None);
        index.insert("thing", "split-owner", MatchKind::Split, None);
        index
    }

    #[test]
    fn a_more_direct_match_wins() {
        let index = index();
        let best = index.best_match("thing", |_| true).unwrap();
        assert_eq!(best.pkgbase, "split-owner");
        assert_eq!(best.kind, MatchKind::Split);
    }

    #[test]
    fn equally_direct_matches_break_ties_by_pkgbase() {
        let mut index = SatisfyIndex::new();
        index.insert("thing", "provider-b", MatchKind::Provides, None);
        index.insert("thing", "provider-a", MatchKind::Provides, None);
        assert_eq!(
            index.best_match("thing", |_| true).unwrap().pkgbase,
            "provider-a"
        );
    }

    #[test]
    fn an_unknown_name_has_no_match() {
        assert!(index().best_match("absent", |_| true).is_none());
    }

    #[test]
    fn the_filter_can_reject_the_most_direct_candidate() {
        let mut index = SatisfyIndex::new();
        index.insert("thing", "old", MatchKind::Name, Some("1.0".into()));
        index.insert("thing", "new", MatchKind::Provides, Some("3.0".into()));

        // Unfiltered, the exact-name match wins despite being older.
        assert_eq!(index.best_match("thing", |_| true).unwrap().pkgbase, "old");
        // Filtered on the constraint, only the newer one qualifies.
        assert_eq!(
            index
                .best_match("thing", |m| m.satisfies(">=2.0"))
                .unwrap()
                .pkgbase,
            "new"
        );
    }

    /// A source that records no version cannot claim to meet a bound, but is
    /// still a perfectly good answer to an unversioned dependency.
    #[test]
    fn a_versionless_match_answers_only_unversioned_dependencies() {
        let mut index = SatisfyIndex::new();
        index.insert("thing", "owner", MatchKind::Name, None);
        assert!(index.best_match("thing", |m| m.satisfies("")).is_some());
        assert!(
            index
                .best_match("thing", |m| m.satisfies(">=1.0"))
                .is_none()
        );
    }
}
