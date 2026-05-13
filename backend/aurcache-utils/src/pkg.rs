use std::cmp::Ordering;

/// Split a dependency string into (name, version_constraint).
/// e.g. "glibc>=2.35" -> ("glibc", ">=2.35")
/// e.g. "python" -> ("python", "")
pub fn parse_dep(dep: &str) -> (&str, &str) {
    let dep = dep.trim();
    for &op in &[">=", "<=", "=", ">", "<"] {
        if let Some(pos) = dep.find(op) {
            let name = dep[..pos].trim();
            let constraint = dep[pos..].trim();
            return (name, constraint);
        }
    }
    (dep, "")
}

/// Pacman-style version comparison.
/// Compares versions like "1.0-1", "2.0.3", "1:2.0".
/// Returns Ordering::Less, Equal, or Greater.
///
/// Algorithm (from libalpm/version.c):
/// 1. Compare epoch (separator `:`)
/// 2. Split pkgver-pkgrel into alternating digit/non-digit segments
/// 3. Compare digit segments numerically, non-digit segments lexicographically
pub fn vercmp(a: &str, b: &str) -> Ordering {
    let (epoch_a, rest_a) = split_epoch(a);
    let (epoch_b, rest_b) = split_epoch(b);

    let epoch_cmp = epoch_a.cmp(&epoch_b);
    if epoch_cmp != Ordering::Equal {
        return epoch_cmp;
    }

    let (ver_a, _rel_a) = split_pkgrel(rest_a);
    let (ver_b, _rel_b) = split_pkgrel(rest_b);

    let ver_cmp = rpmvercmp(ver_a, ver_b);
    if ver_cmp != Ordering::Equal {
        return ver_cmp;
    }

    // If versions equal, compare pkgrel (without the leading `-`)
    let rel_a = rest_a.trim_start_matches(|c| c != '-');
    let rel_b = rest_b.trim_start_matches(|c| c != '-');
    rpmvercmp(rel_a, rel_b)
}

fn split_epoch(v: &str) -> (u64, &str) {
    if let Some(pos) = v.find(':') {
        let epoch: u64 = v[..pos].parse().unwrap_or(0);
        (epoch, &v[pos + 1..])
    } else {
        (0, v)
    }
}

fn split_pkgrel(v: &str) -> (&str, &str) {
    if let Some(pos) = v.rfind('-') {
        (&v[..pos], &v[pos..])
    } else {
        (v, "")
    }
}

/// rpmlib-style version comparison used by pacman.
fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a.is_empty() && b.is_empty() {
        return Ordering::Equal;
    }
    if a.is_empty() {
        return Ordering::Less;
    }
    if b.is_empty() {
        return Ordering::Greater;
    }

    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let mut i = 0;
    let mut j = 0;

    while i < a_bytes.len() || j < b_bytes.len() {
        let mut segment_type = None;
        if i < a_bytes.len() {
            segment_type = Some(if a_bytes[i].is_ascii_digit() {
                SegmentType::Digit
            } else {
                SegmentType::Alpha
            });
        } else if j < b_bytes.len() {
            segment_type = Some(if b_bytes[j].is_ascii_digit() {
                SegmentType::Digit
            } else {
                SegmentType::Alpha
            });
        }

        match segment_type {
            Some(SegmentType::Digit) => {
                let (num_a, len_a) = read_number(a_bytes, i);
                let (num_b, len_b) = read_number(b_bytes, j);
                let cmp = num_a.cmp(&num_b);
                if cmp != Ordering::Equal {
                    return cmp;
                }
                i += len_a;
                j += len_b;
            }
            Some(SegmentType::Alpha) => {
                let (seg_a, len_a) = read_alpha(a_bytes, i);
                let (seg_b, len_b) = read_alpha(b_bytes, j);
                let cmp = seg_a.cmp(seg_b);
                if cmp != Ordering::Equal {
                    return cmp;
                }
                i += len_a;
                j += len_b;
            }
            None => break,
        }

        // Handle separators (non-digit, non-alpha chars like '.', '_', '~', etc.)
        // '~' means a segment of '~' sorts before any other segment
        while i < a_bytes.len() && !a_bytes[i].is_ascii_alphanumeric() {
            i += 1;
        }
        while j < b_bytes.len() && !b_bytes[j].is_ascii_alphanumeric() {
            j += 1;
        }
    }

    Ordering::Equal
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SegmentType {
    Digit,
    Alpha,
}

fn read_number(bytes: &[u8], start: usize) -> (u64, usize) {
    let mut val: u64 = 0;
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        val = val
            .saturating_mul(10)
            .saturating_add((bytes[i] - b'0') as u64);
        i += 1;
    }
    (val, i - start)
}

fn read_alpha(bytes: &[u8], start: usize) -> (&str, usize) {
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    // Also include non-alphanumeric non-digit chars (except separators)
    while i < bytes.len()
        && !bytes[i].is_ascii_alphanumeric()
        && bytes[i] != b'.'
        && bytes[i] != b'-'
        && bytes[i] != b'_'
        && bytes[i] != b'~'
    {
        i += 1;
    }
    // SAFETY: valid UTF-8 since we're scanning ASCII bytes
    let s = unsafe { std::str::from_utf8_unchecked(&bytes[start..i]) };
    (s, i - start)
}

/// Check if a built version satisfies a version constraint.
/// `built_version` is the version string from a successful build.
/// `constraint` is e.g. ">=2.0", "=1.5", "<3.0", or "" (unconstrained).
pub fn satisfies_constraint(built_version: &str, constraint: &str) -> bool {
    let constraint = constraint.trim();
    if constraint.is_empty() {
        return true;
    }

    // Identify operator
    let (op, required_ver) = if constraint.starts_with(">=") {
        (">=", &constraint[2..])
    } else if constraint.starts_with("<=") {
        ("<=", &constraint[2..])
    } else if constraint.starts_with("=") {
        ("=", &constraint[1..])
    } else if constraint.starts_with(">") {
        (">", &constraint[1..])
    } else if constraint.starts_with("<") {
        ("<", &constraint[1..])
    } else {
        // No operator means exact match (though this is unusual)
        ("=", constraint)
    };

    let required_ver = required_ver.trim();
    if required_ver.is_empty() {
        return true;
    }

    let cmp = vercmp(built_version, required_ver);
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
