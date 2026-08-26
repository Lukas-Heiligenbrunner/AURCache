use std::cmp::Ordering;
use std::collections::HashMap;
use std::str::FromStr;

use alpm_types::{Version, VersionRequirement};

pub use aurcache_deps::parse_dep;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constraint(pub VersionRequirement);

impl Constraint {
    pub fn is_satisfied(&self, version: &Version) -> bool {
        self.0.is_satisfied_by(version)
    }
}

impl std::fmt::Display for Constraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Pacman-style version comparison using `alpm-types`.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    let a_ver = Version::from_str(a);
    let b_ver = Version::from_str(b);
    match (a_ver, b_ver) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => Ordering::Equal,
    }
}

/// Check if a built version satisfies a version constraint stored as a plain string.
pub fn satisfies_constraint(built_version: &str, constraint: &str) -> bool {
    let constraint = constraint.trim();
    if constraint.is_empty() {
        return true;
    }
    let Ok(built) = Version::from_str(built_version) else {
        return false;
    };
    let Ok(req) = VersionRequirement::from_str(constraint) else {
        return false;
    };
    req.is_satisfied_by(&built)
}

/// Insert a dependency constraint into a map. For constraints in the same
/// direction (both lower or both upper bounds) the stricter one is kept.
/// When directions differ (which would require a Range), the existing entry wins.
pub fn merge_constraint_into(
    constraints: &mut HashMap<String, Option<Constraint>>,
    name: &str,
    constraint: Option<Constraint>,
) -> anyhow::Result<()> {
    use alpm_types::VersionComparison::{Greater, GreaterOrEqual, Less, LessOrEqual};
    let merged = match (constraints.remove(name).flatten(), constraint) {
        (None, new) => new,
        (existing, None) => existing,
        (Some(lhs), Some(rhs)) => {
            let l = &lhs.0;
            let r = &rhs.0;
            Some(match (l.comparison, r.comparison) {
                (GreaterOrEqual | Greater, GreaterOrEqual | Greater) => {
                    if r.version > l.version {
                        rhs
                    } else {
                        lhs
                    }
                }
                (LessOrEqual | Less, LessOrEqual | Less) => {
                    if r.version < l.version {
                        rhs
                    } else {
                        lhs
                    }
                }
                _ => anyhow::bail!(
                    "conflicting constraints for '{name}': '{lhs}' and '{rhs}' bound in opposite directions"
                ),
            })
        }
    };
    constraints.insert(name.to_string(), merged);
    Ok(())
}

pub fn parse_dep_constraint(constraint: &str) -> Option<Constraint> {
    let constraint = constraint.trim();
    if constraint.is_empty() {
        return None;
    }
    VersionRequirement::from_str(constraint)
        .ok()
        .map(Constraint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vercmp_equal() {
        assert_eq!(vercmp("1.0", "1.0"), Ordering::Equal);
        assert_eq!(vercmp("2.0.1", "2.0.1"), Ordering::Equal);
        assert_eq!(vercmp("1.0-1", "1.0-1"), Ordering::Equal);
    }

    #[test]
    fn test_vercmp_less() {
        assert_eq!(vercmp("1.0", "2.0"), Ordering::Less);
        assert_eq!(vercmp("1.0", "1.1"), Ordering::Less);
        assert_eq!(vercmp("1.0", "1.0.1"), Ordering::Less);
        assert_eq!(vercmp("1.0-1", "1.0-2"), Ordering::Less);
    }

    #[test]
    fn test_vercmp_greater() {
        assert_eq!(vercmp("2.0", "1.0"), Ordering::Greater);
        assert_eq!(vercmp("1.1", "1.0"), Ordering::Greater);
        assert_eq!(vercmp("1.10", "1.9"), Ordering::Greater);
    }

    #[test]
    fn test_vercmp_epoch() {
        assert_eq!(vercmp("1:1.0", "1:1.0"), Ordering::Equal);
        assert_eq!(vercmp("2:1.0", "1:1.0"), Ordering::Greater);
        assert_eq!(vercmp("1:2.0", "1:1.0"), Ordering::Greater);
    }

    #[test]
    fn test_vercmp_pkgrel() {
        assert_eq!(vercmp("1.0-1", "1.0"), Ordering::Greater);
        assert_eq!(vercmp("1.0", "1.0-1"), Ordering::Less);
        assert_eq!(vercmp("1.0-2", "1.0-1"), Ordering::Greater);
        assert_eq!(vercmp("1.0-1", "1.0-2"), Ordering::Less);
    }

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

    #[test]
    fn test_merge_constraint_into_last_wins() {
        let mut constraints = HashMap::new();
        merge_constraint_into(&mut constraints, "glibc", parse_dep_constraint(">=2.0")).unwrap();
        merge_constraint_into(&mut constraints, "glibc", parse_dep_constraint(">=3.0")).unwrap();

        assert_eq!(
            constraints
                .get("glibc")
                .cloned()
                .flatten()
                .map(|c| c.to_string())
                .unwrap_or_default(),
            ">=3.0"
        );
    }
}

/// The alpm architectures a package's configured platforms correspond to.
///
/// `packages.platforms` is a semicolon-delimited list of this project's
/// `Platform`; `.SRCINFO` is keyed by alpm's `SystemArchitecture`. Dependency
/// extraction needs the second, so the two have to be bridged somewhere.
///
/// Unparseable entries are skipped rather than failing: a bad platform string
/// should not stop a package's dependency graph from being computed for the
/// platforms that *are* valid.
#[must_use]
pub fn architectures_for_platforms(platforms: &str) -> Vec<alpm_types::SystemArchitecture> {
    use alpm_types::SystemArchitecture;
    use pacman_mirrors::platforms::Platform;

    Platform::parse_many(platforms)
        .filter_map(Result::ok)
        .map(|platform| match platform {
            Platform::X86_64 => SystemArchitecture::X86_64,
            Platform::Aarch64 => SystemArchitecture::Aarch64,
            Platform::Armv7h => SystemArchitecture::Armv7h,
        })
        .collect()
}

#[cfg(test)]
mod architecture_tests {
    use super::architectures_for_platforms;
    use alpm_types::SystemArchitecture;

    #[test]
    fn each_platform_maps_to_its_alpm_architecture() {
        assert_eq!(
            architectures_for_platforms("x86_64;aarch64;armv7h"),
            vec![
                SystemArchitecture::X86_64,
                SystemArchitecture::Aarch64,
                SystemArchitecture::Armv7h,
            ]
        );
    }

    /// A package built only for aarch64 must not be described by x86_64's
    /// dependency list, which is what the hardcoded architecture used to do.
    #[test]
    fn a_single_platform_does_not_pull_in_x86_64() {
        assert_eq!(
            architectures_for_platforms("aarch64"),
            vec![SystemArchitecture::Aarch64]
        );
    }

    /// One bad entry must not lose the good ones.
    #[test]
    fn unparseable_platforms_are_skipped() {
        assert_eq!(
            architectures_for_platforms("x86_64;sparc;aarch64"),
            vec![SystemArchitecture::X86_64, SystemArchitecture::Aarch64]
        );
        assert!(architectures_for_platforms("").is_empty());
    }
}
