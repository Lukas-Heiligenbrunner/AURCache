//! Pacman-style version comparison.
//!
//! This lives at the bottom of the crate stack because two layers need it and
//! neither can see the other's copy: dependency resolution here has to know
//! whether a repository entry actually satisfies a constraint, and the build
//! queue in `aurcache-utils` has to know whether a dependency's latest build
//! does. `aurcache-utils` re-exports this rather than keeping a second
//! implementation.

use std::str::FromStr;

use alpm_types::{Version, VersionRequirement};

/// Whether `version` satisfies `constraint`: one pacman constraint string
/// such as `">=2.0"`, or several comma-joined (`">=1.0,<2.0"`), every one of
/// which must hold.
///
/// The comma form is what `aurcache_utils::pkg` stores when one dependency
/// accumulates bounds in both directions — a single PKGBUILD declaring
/// `foo>=1.0` and `foo<2.0` is a range, not a conflict. Plain pacman strings
/// never contain a comma, so single bounds are unaffected.
///
/// An empty constraint is satisfied by any version -- that is how an
/// unversioned `depends` entry is stored. A version or constraint that
/// `alpm-types` cannot parse is treated as *not* satisfied: refusing to
/// promote a build is recoverable, promoting one against a version nobody
/// could parse is not.
#[must_use]
pub fn satisfies_constraint(version: &str, constraint: &str) -> bool {
    let constraint = constraint.trim();
    if constraint.is_empty() {
        return true;
    }
    let Ok(version) = Version::from_str(version) else {
        return false;
    };
    constraint.split(',').all(|bound| {
        let bound = bound.trim();
        // An empty element (a stray trailing comma) constrains nothing;
        // anything else must parse and hold.
        bound.is_empty()
            || VersionRequirement::from_str(bound)
                .is_ok_and(|requirement| requirement.is_satisfied_by(&version))
    })
}

#[cfg(test)]
mod tests {
    use super::satisfies_constraint;

    #[test]
    fn test_satisfies_constraint() {
        assert!(satisfies_constraint("2.0", ">=1.0"));
        assert!(satisfies_constraint("2.0", ">=2.0"));
        assert!(!satisfies_constraint("1.0", ">=2.0"));
        assert!(satisfies_constraint("1.0", "<=2.0"));
        assert!(satisfies_constraint("2.0", "<=2.0"));
        assert!(!satisfies_constraint("3.0", "<=2.0"));
        assert!(satisfies_constraint("1.5", "=1.5"));
        assert!(!satisfies_constraint("1.6", "=1.5"));
        assert!(satisfies_constraint("2.0", ">1.0"));
        assert!(!satisfies_constraint("1.0", ">1.0"));
        assert!(satisfies_constraint("1.0", "<2.0"));
        assert!(!satisfies_constraint("2.0", "<2.0"));
        assert!(satisfies_constraint("2.0", ""));
        assert!(satisfies_constraint("2.0", ">=1.0-2"));
    }

    /// A repository entry whose version is unparseable must not be allowed to
    /// answer a versioned dependency.
    #[test]
    fn unparseable_versions_do_not_satisfy() {
        assert!(!satisfies_constraint("not a version", ">=1.0"));
        assert!(!satisfies_constraint("1.0", "not a constraint"));
        // ...but an unversioned dependency is satisfied regardless, since
        // nothing about the version was ever asked.
        assert!(satisfies_constraint("not a version", ""));
    }

    /// A range both bounds must hold: inside is fine, either side is not,
    /// and one unparseable element fails the whole conjunction rather than
    /// being skipped.
    #[test]
    fn conjoined_bounds_all_hold() {
        assert!(satisfies_constraint("1.5", ">=1.0,<2.0"));
        assert!(!satisfies_constraint("2.5", ">=1.0,<2.0"));
        assert!(!satisfies_constraint("0.5", ">=1.0,<2.0"));
        assert!(!satisfies_constraint("1.5", ">=1.0,not a constraint"));
    }
}
