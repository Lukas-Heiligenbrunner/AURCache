use std::cmp::Ordering;
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
/// `constraint` is e.g. ">=2.0", "=1.5", "<3.0", or "" (unconstrained).
pub fn satisfies_constraint(built_version: &str, constraint: &str) -> bool {
    let constraint = constraint.trim();
    if constraint.is_empty() {
        return true;
    }

    let (op, required_ver) = if let Some(stripped) = constraint.strip_prefix(">=") {
        (">=", stripped)
    } else if let Some(stripped) = constraint.strip_prefix("<=") {
        ("<=", stripped)
    } else if let Some(stripped) = constraint.strip_prefix("=") {
        ("=", stripped)
    } else if let Some(stripped) = constraint.strip_prefix(">") {
        (">", stripped)
    } else if let Some(stripped) = constraint.strip_prefix("<") {
        ("<", stripped)
    } else {
        ("=", constraint)
    };

    let required_ver = required_ver.trim();
    if required_ver.is_empty() {
        return true;
    }

    let built = match alpm_types::Version::from_str(built_version) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let required = match alpm_types::Version::from_str(required_ver) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let cmp = built.cmp(&required);
    match op {
        ">=" => cmp != Ordering::Less,
        "<=" => cmp != Ordering::Greater,
        "=" => cmp == Ordering::Equal,
        ">" => cmp == Ordering::Greater,
        "<" => cmp == Ordering::Less,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dep_no_constraint() {
        assert_eq!(parse_dep("glibc"), ("glibc", ""));
        assert_eq!(parse_dep("  python  "), ("python", ""));
    }

    #[test]
    fn test_parse_dep_with_constraint() {
        assert_eq!(parse_dep("glibc>=2.35"), ("glibc", ">=2.35"));
        assert_eq!(parse_dep("cmake<=3.20"), ("cmake", "<=3.20"));
        assert_eq!(parse_dep("pkg=1.5"), ("pkg", "=1.5"));
        assert_eq!(parse_dep("lib>2.0"), ("lib", ">2.0"));
        assert_eq!(parse_dep("libfoo<3"), ("libfoo", "<3"));
    }

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
}
