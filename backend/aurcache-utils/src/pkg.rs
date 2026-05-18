use std::cmp::Ordering;
use std::collections::HashMap;
use std::str::FromStr;

pub use aurcache_deps::parse_dep;

/// Pacman-style version comparison using `alpm-types`.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    let a_ver = alpm_types::Version::from_str(a);
    let b_ver = alpm_types::Version::from_str(b);
    match (a_ver, b_ver) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => Ordering::Equal,
    }
}

/// Check if a built version satisfies a version constraint.
/// `built_version` is the version string from a successful build.
/// `constraint` is e.g. ">=2.0", "=1.5", "<3.0", ">=2.0,<4.0", or "" (unconstrained).
pub fn satisfies_constraint(built_version: &str, constraint: &str) -> bool {
    let constraints = split_constraints(constraint);
    if constraints.is_empty() {
        return true;
    }
    let Ok(built) = alpm_types::Version::from_str(built_version) else {
        return false;
    };

    constraints.into_iter().all(|constraint| {
        let Ok(req) = alpm_types::VersionRequirement::from_str(constraint) else {
            return false;
        };
        req.is_satisfied_by(&built)
    })
}

/// Merge two version constraints into a single comma-separated requirement list.
pub fn merge_version_constraints(existing: &str, new: &str) -> String {
    let mut merged: Vec<String> = Vec::new();

    for constraint in split_constraints(existing)
        .into_iter()
        .chain(split_constraints(new))
    {
        if !merged.iter().any(|seen| seen == constraint) {
            merged.push(constraint.to_string());
        }
    }

    merged.join(",")
}

/// Insert a dependency constraint into a map, merging it with any existing one.
pub fn merge_constraint_into(
    constraints: &mut HashMap<String, String>,
    name: &str,
    constraint: &str,
) {
    constraints
        .entry(name.to_string())
        .and_modify(|existing| {
            *existing = merge_version_constraints(existing.as_str(), constraint);
        })
        .or_insert_with(|| constraint.to_string());
}

fn split_constraints(constraint: &str) -> Vec<&str> {
    constraint
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
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
        assert!(satisfies_constraint("2.5", ">=2.0,<3.0"));
        assert!(!satisfies_constraint("3.0", ">=2.0,<3.0"));
    }

    #[test]
    fn test_merge_version_constraints() {
        assert_eq!(merge_version_constraints("", ""), "");
        assert_eq!(merge_version_constraints(">=1.0", ""), ">=1.0");
        assert_eq!(merge_version_constraints("", ">=1.0"), ">=1.0");
        assert_eq!(merge_version_constraints(">=1.0", ">=2.0"), ">=1.0,>=2.0");
        assert_eq!(
            merge_version_constraints(">=1.0,<4.0", "<=3.0"),
            ">=1.0,<4.0,<=3.0"
        );
        assert_eq!(
            merge_version_constraints(">=1.0", ">=1.0,<4.0"),
            ">=1.0,<4.0"
        );
    }

    #[test]
    fn test_merge_constraint_into() {
        let mut constraints = HashMap::new();
        merge_constraint_into(&mut constraints, "glibc", ">=2.0");
        merge_constraint_into(&mut constraints, "glibc", "<3.0");

        assert_eq!(constraints.get("glibc"), Some(&">=2.0,<3.0".to_string()));
    }
}
