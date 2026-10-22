use std::cmp::Ordering;
use std::collections::HashMap;
use std::str::FromStr;

use alpm_types::{Version, VersionRequirement};
use aurcache_db::lists::Platforms;

pub use aurcache_deps::{parse_dep, satisfies_constraint};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constraint(pub VersionRequirement);

impl std::fmt::Display for Constraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Pacman-style version comparison using `alpm-types`.
///
/// Returns `None` when either side is not a valid alpm version, so callers have
/// to decide what an uncomparable pair means for them. Reporting that as
/// `Equal` would be worse than useless: "not newer" is exactly what suppresses
/// an update, so an unparseable version would silently freeze a package
/// forever.
pub fn vercmp(a: &str, b: &str) -> Option<Ordering> {
    match (Version::from_str(a), Version::from_str(b)) {
        (Ok(a), Ok(b)) => Some(a.cmp(&b)),
        _ => None,
    }
}

/// Insert a dependency bound into the conjunction for `name`.
///
/// Same-direction bounds tighten: only the strictest lower and the strictest
/// upper bound survive (a strict operator wins a version tie, so `>1.0`
/// replaces `>=1.0` rather than the other way round). Bounds in opposite
/// directions accumulate — one package declaring `foo>=1.0` and `foo<2.0` is
/// a range, and rejecting it rejects a valid package. A conjunction that is
/// statically empty (`>=2.0` with `<1.0`) still fails loudly: nothing could
/// ever satisfy it, so building anyway would only waste a build.
pub fn merge_constraint_into(
    constraints: &mut HashMap<String, Vec<Constraint>>,
    name: &str,
    constraint: Option<Constraint>,
) -> anyhow::Result<()> {
    use alpm_types::VersionComparison::Equal;
    let Some(bound) = constraint else {
        constraints.entry(name.to_string()).or_default();
        return Ok(());
    };
    let conjunction = constraints.entry(name.to_string()).or_default();

    // An exact pin subsumes every bound it is consistent with, and contradicts
    // the rest — decide it against the conjunction as a whole.
    if bound.0.comparison == Equal {
        for existing in conjunction.iter() {
            if !bound_admits_version(existing, &bound.0.version) {
                anyhow::bail!(
                    "conflicting constraints for '{name}': '{existing}' excludes '= {}'",
                    bound.0.version
                );
            }
        }
        conjunction.clear();
        conjunction.push(bound);
        return Ok(());
    }
    if let Some(pin) = conjunction.iter().find(|b| b.0.comparison == Equal) {
        if !bound_admits_version(&bound, &pin.0.version) {
            anyhow::bail!("conflicting constraints for '{name}': '{bound}' excludes '{pin}'");
        }
        // The pin stands; the new bound adds nothing.
        return Ok(());
    }

    let lower = is_lower(&bound.0);
    if conjunction
        .iter()
        .filter(|b| is_lower(&b.0) == lower)
        .any(|b| at_least_as_strict(&b.0, &bound.0))
    {
        // Something already here admits no more than the new bound does.
        return Ok(());
    }
    conjunction.retain(|b| is_lower(&b.0) != lower);
    conjunction.push(bound);
    if let Some(conflict) = empty_conjunction(name, conjunction) {
        return Err(conflict);
    }
    Ok(())
}

/// Merge every bound one dependency name declares onto `pkgbase`'s entry.
///
/// One name can carry a whole range, so each bound merges in turn. An
/// unversioned name has no bounds at all, and still records `pkgbase`: the
/// entry is the edge, and a loop over the bounds alone would drop it.
pub fn merge_bounds_into(
    constraints: &mut HashMap<String, Vec<Constraint>>,
    pkgbase: &str,
    bounds: Option<&Vec<Constraint>>,
) -> anyhow::Result<()> {
    constraints.entry(pkgbase.to_string()).or_default();
    for bound in bounds.into_iter().flatten() {
        merge_constraint_into(constraints, pkgbase, Some(bound.clone()))?;
    }
    Ok(())
}

/// Whether `bound` is a lower (`>`/`>=`) rather than an upper (`<`/`<=`)
/// bound. Exact pins never reach here; see [`merge_constraint_into`].
fn is_lower(bound: &alpm_types::VersionRequirement) -> bool {
    use alpm_types::VersionComparison::{Greater, GreaterOrEqual};
    matches!(bound.comparison, Greater | GreaterOrEqual)
}

/// Whether `keeper` admits no version `candidate` does not, for two bounds
/// in the same direction: a higher lower bound (a lower upper bound) wins,
/// and a strict operator wins a version tie.
fn at_least_as_strict(
    keeper: &alpm_types::VersionRequirement,
    candidate: &alpm_types::VersionRequirement,
) -> bool {
    use alpm_types::VersionComparison::{Greater, GreaterOrEqual, Less, LessOrEqual};
    let ordering = keeper.version.partial_cmp(&candidate.version);
    if is_lower(keeper) {
        match ordering {
            Some(std::cmp::Ordering::Greater) => true,
            Some(std::cmp::Ordering::Equal) => {
                matches!(keeper.comparison, Greater)
                    || matches!(candidate.comparison, GreaterOrEqual)
            }
            _ => false,
        }
    } else {
        match ordering {
            Some(std::cmp::Ordering::Less) => true,
            Some(std::cmp::Ordering::Equal) => {
                matches!(keeper.comparison, Less) || matches!(candidate.comparison, LessOrEqual)
            }
            _ => false,
        }
    }
}

/// Whether the single bound `bound` admits `version` — the static half of a
/// pin-consistency check. An uncomparable pair admits: the runtime check
/// decides those, and merge must not fail a package over versions it cannot
/// even order.
fn bound_admits_version(bound: &Constraint, version: &alpm_types::Version) -> bool {
    use alpm_types::VersionComparison::{Equal, Greater, GreaterOrEqual, Less, LessOrEqual};
    // Ordered as bound-version against the candidate: `>=1.0` admits 1.5
    // because 1.0 orders below it.
    let ordering = bound.0.version.partial_cmp(version);
    match bound.0.comparison {
        Greater => !matches!(
            ordering,
            Some(std::cmp::Ordering::Equal | std::cmp::Ordering::Greater)
        ),
        GreaterOrEqual => !matches!(ordering, Some(std::cmp::Ordering::Greater)),
        Less => !matches!(
            ordering,
            Some(std::cmp::Ordering::Equal | std::cmp::Ordering::Less)
        ),
        LessOrEqual => !matches!(ordering, Some(std::cmp::Ordering::Less)),
        Equal => matches!(ordering, Some(std::cmp::Ordering::Equal) | None),
    }
}

/// The tightest lower and upper bounds, when the conjunction provably admits
/// nothing: a lower bound above the upper one, or equal bounds that exclude
/// the shared version between them (`>1.0` with `<=1.0`). `None` when
/// versions cannot be ordered or the range is non-empty. Exact pins never
/// reach here; merge decides those directly.
fn empty_conjunction(name: &str, conjunction: &[Constraint]) -> Option<anyhow::Error> {
    use std::cmp::Ordering;
    let mut lower: Option<&alpm_types::VersionRequirement> = None;
    let mut upper: Option<&alpm_types::VersionRequirement> = None;
    for bound in conjunction {
        let slot = if is_lower(&bound.0) {
            &mut lower
        } else {
            &mut upper
        };
        let replace = match slot {
            None => true,
            Some(current) => !at_least_as_strict(current, &bound.0),
        };
        if replace {
            *slot = Some(&bound.0);
        }
    }
    let (lower, upper) = (lower?, upper?);
    match lower.version.partial_cmp(&upper.version) {
        Some(Ordering::Greater) => Some(anyhow::anyhow!(
            "conflicting constraints for '{name}': '{lower}' excludes '{upper}'"
        )),
        Some(Ordering::Equal)
            if lower.comparison != alpm_types::VersionComparison::GreaterOrEqual
                || upper.comparison != alpm_types::VersionComparison::LessOrEqual =>
        {
            Some(anyhow::anyhow!(
                "conflicting constraints for '{name}': '{lower}' excludes '{upper}'"
            ))
        }
        _ => None,
    }
}

/// The stored form of merged bounds: comma-joined single requirements, which
/// [`aurcache_deps::satisfies_constraint`] checks one by one. Empty (an
/// unversioned dependency) stores as the empty string, as before.
#[must_use]
pub fn join_constraints(bounds: &[Constraint]) -> String {
    bounds
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Every name a package answers to itself: its pkgbase, its split packages,
/// and what it provides (a versioned `provides` contributes the name).
///
/// A dependency on one of these is satisfied by the package's own artifacts,
/// so the add and resync paths leave it out before resolution -- asked
/// elsewhere, a co-provider would win and the package would grow an edge to
/// it.
#[must_use]
pub fn self_provided_names(
    pkgbase: &str,
    pkgnames: &[String],
    provides: &[String],
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::from([pkgbase.to_string()]);
    names.extend(pkgnames.iter().cloned());
    names.extend(
        provides
            .iter()
            .map(|provide| parse_dep(provide).name)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
    );
    names
}

/// The dependencies a package declares.
///
/// The single answer to "what does this pkgbase need?". Both the add path and
/// the resync path used to derive this, from the same `PkgDeps`, with their
/// own near-copies of the merge loop — and only one of them kept the declared
/// order.
pub struct DependencySet {
    /// Names in declared order, deduplicated.
    ///
    /// The order is load-bearing: it decides the order dependencies are
    /// planned and therefore the order their builds are enqueued. Iterating
    /// `constraints` instead would vary between processes and make identical
    /// input produce different build orders.
    ///
    /// One name can carry several bounds (a range declared as two entries);
    /// see [`merge_constraint_into`].
    pub names: Vec<String>,
    /// The merged bounds for each name: several when a range was declared,
    /// empty for an unversioned dependency.
    pub constraints: HashMap<String, Vec<Constraint>>,
}

impl DependencySet {
    /// Runtime and build-time dependencies together: AURCache needs both
    /// present before it can build anything.
    pub fn of(deps: &aurcache_deps::PkgDeps) -> anyhow::Result<Self> {
        Self::parse(deps.depends.iter().chain(deps.make_depends.iter()))
    }

    /// Parse and merge raw `name>=version` strings.
    pub fn parse<'a>(deps: impl Iterator<Item = &'a String>) -> anyhow::Result<Self> {
        let mut set = Self {
            names: Vec::new(),
            constraints: HashMap::new(),
        };
        for dep in deps {
            let dep = parse_dep(dep);
            // The constraint map's keys are exactly the dependency set, so a
            // membership check there is the dedupe.
            if !set.constraints.contains_key(dep.name) {
                set.names.push(dep.name.to_string());
            }
            merge_constraint_into(
                &mut set.constraints,
                dep.name,
                parse_dep_constraint(dep.constraint),
            )?;
        }
        Ok(set)
    }

    /// Every dependency in declared order, with its constraint in the plain
    /// string form that both resolution and the `dependencies` rows use.
    #[must_use]
    pub fn declared(&self) -> Vec<Declared> {
        self.names
            .iter()
            .map(|name| Declared {
                name: name.clone(),
                constraint: self
                    .constraints
                    .get(name)
                    .map(|bounds| join_constraints(bounds))
                    .unwrap_or_default(),
            })
            .collect()
    }
}

/// One declared dependency, owning what an [`aurcache_deps::Dependency`]
/// borrows.
pub struct Declared {
    pub name: String,
    /// Comma-joined when a range was declared; empty when unversioned.
    pub constraint: String,
}

impl Declared {
    /// This dependency as resolver input.
    #[must_use]
    pub fn as_dependency(&self) -> aurcache_deps::Dependency<'_> {
        aurcache_deps::Dependency::new(&self.name, &self.constraint)
    }
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
        assert_eq!(vercmp("1.0", "1.0"), Some(Ordering::Equal));
        assert_eq!(vercmp("2.0.1", "2.0.1"), Some(Ordering::Equal));
        assert_eq!(vercmp("1.0-1", "1.0-1"), Some(Ordering::Equal));
    }

    #[test]
    fn test_vercmp_less() {
        assert_eq!(vercmp("1.0", "2.0"), Some(Ordering::Less));
        assert_eq!(vercmp("1.0", "1.1"), Some(Ordering::Less));
        assert_eq!(vercmp("1.0", "1.0.1"), Some(Ordering::Less));
        assert_eq!(vercmp("1.0-1", "1.0-2"), Some(Ordering::Less));
    }

    #[test]
    fn test_vercmp_greater() {
        assert_eq!(vercmp("2.0", "1.0"), Some(Ordering::Greater));
        assert_eq!(vercmp("1.1", "1.0"), Some(Ordering::Greater));
        assert_eq!(vercmp("1.10", "1.9"), Some(Ordering::Greater));
    }

    #[test]
    fn test_vercmp_epoch() {
        assert_eq!(vercmp("1:1.0", "1:1.0"), Some(Ordering::Equal));
        assert_eq!(vercmp("2:1.0", "1:1.0"), Some(Ordering::Greater));
        assert_eq!(vercmp("1:2.0", "1:1.0"), Some(Ordering::Greater));
    }

    #[test]
    fn test_vercmp_pkgrel() {
        assert_eq!(vercmp("1.0-1", "1.0"), Some(Ordering::Greater));
        assert_eq!(vercmp("1.0", "1.0-1"), Some(Ordering::Less));
        assert_eq!(vercmp("1.0-2", "1.0-1"), Some(Ordering::Greater));
        assert_eq!(vercmp("1.0-1", "1.0-2"), Some(Ordering::Less));
    }

    /// An unparseable version is reported as uncomparable rather than as
    /// "equal", which callers would read as "not newer" and never update.
    #[test]
    fn test_vercmp_unparseable_is_uncomparable() {
        assert_eq!(vercmp("not a version!", "1.0"), None);
        assert_eq!(vercmp("1.0", "not a version!"), None);
    }

    fn merged_string(constraints: &HashMap<String, Vec<Constraint>>, name: &str) -> String {
        constraints
            .get(name)
            .map(|bounds| join_constraints(bounds))
            .unwrap_or_default()
    }

    #[test]
    fn test_merge_constraint_into_last_wins() {
        let mut constraints = HashMap::new();
        merge_constraint_into(&mut constraints, "glibc", parse_dep_constraint(">=2.0")).unwrap();
        merge_constraint_into(&mut constraints, "glibc", parse_dep_constraint(">=3.0")).unwrap();

        assert_eq!(merged_string(&constraints, "glibc"), ">=3.0");
    }

    /// An unversioned dependency is still a dependency: no bounds must not
    /// mean no entry, or the edge it stands for is never written.
    #[test]
    fn merging_no_bounds_still_records_the_dependency() {
        let mut constraints = HashMap::new();
        merge_bounds_into(&mut constraints, "mydep", None).unwrap();
        merge_bounds_into(&mut constraints, "other", Some(&Vec::new())).unwrap();
        assert_eq!(merged_string(&constraints, "mydep"), "");
        assert!(constraints.contains_key("mydep"));
        assert!(constraints.contains_key("other"));
    }

    /// Bounds in opposite directions are a range, not a conflict: one
    /// package declaring both must stay addable.
    #[test]
    fn test_merge_constraint_into_accumulates_a_range() {
        let mut constraints = HashMap::new();
        merge_constraint_into(&mut constraints, "python", parse_dep_constraint(">=3.11")).unwrap();
        merge_constraint_into(&mut constraints, "python", parse_dep_constraint("<3.13")).unwrap();

        assert_eq!(merged_string(&constraints, "python"), ">=3.11,<3.13");
    }

    /// A strict operator wins a version tie: `>1.0` after `>=1.0` tightens,
    /// and `>=1.0` after `>1.0` adds nothing.
    #[test]
    fn test_merge_constraint_into_strict_wins_ties() {
        let mut constraints = HashMap::new();
        merge_constraint_into(&mut constraints, "foo", parse_dep_constraint(">=1.0")).unwrap();
        merge_constraint_into(&mut constraints, "foo", parse_dep_constraint(">1.0")).unwrap();
        assert_eq!(merged_string(&constraints, "foo"), ">1.0");

        let mut constraints = HashMap::new();
        merge_constraint_into(&mut constraints, "foo", parse_dep_constraint(">1.0")).unwrap();
        merge_constraint_into(&mut constraints, "foo", parse_dep_constraint(">=1.0")).unwrap();
        assert_eq!(merged_string(&constraints, "foo"), ">1.0");
    }

    /// A range nothing can satisfy fails at merge time, not three builds
    /// later when no version ever matches.
    #[test]
    fn test_merge_constraint_into_rejects_empty_ranges() {
        let mut constraints = HashMap::new();
        merge_constraint_into(&mut constraints, "foo", parse_dep_constraint(">=2.0")).unwrap();
        assert!(
            merge_constraint_into(&mut constraints, "foo", parse_dep_constraint("<1.0")).is_err()
        );

        let mut constraints = HashMap::new();
        merge_constraint_into(&mut constraints, "foo", parse_dep_constraint(">1.0")).unwrap();
        assert!(
            merge_constraint_into(&mut constraints, "foo", parse_dep_constraint("<=1.0")).is_err()
        );
    }

    /// An exact pin absorbs the bounds it is consistent with and refuses the
    /// ones it is not.
    #[test]
    fn test_merge_constraint_into_pin() {
        let mut constraints = HashMap::new();
        merge_constraint_into(&mut constraints, "foo", parse_dep_constraint(">=1.0")).unwrap();
        merge_constraint_into(&mut constraints, "foo", parse_dep_constraint("=1.5")).unwrap();
        assert_eq!(merged_string(&constraints, "foo"), "=1.5");

        let mut constraints = HashMap::new();
        merge_constraint_into(&mut constraints, "foo", parse_dep_constraint("=1.5")).unwrap();
        merge_constraint_into(&mut constraints, "foo", parse_dep_constraint("<1.0")).unwrap_err();
    }
}

/// The alpm architectures a package's configured platforms correspond to.
///
/// `packages.platforms` holds this project's `Platform`s; `.SRCINFO` is keyed by
/// alpm's `SystemArchitecture`. Dependency extraction needs the second, so the
/// two have to be bridged somewhere.
#[must_use]
pub fn architectures(platforms: &Platforms) -> Vec<alpm_types::SystemArchitecture> {
    platforms
        .as_slice()
        .iter()
        .copied()
        .map(architecture)
        .collect()
}

/// The alpm architecture a platform builds for.
#[must_use]
pub const fn architecture(
    platform: pacman_mirrors::platforms::Platform,
) -> alpm_types::SystemArchitecture {
    use alpm_types::SystemArchitecture;
    use pacman_mirrors::platforms::Platform;
    match platform {
        Platform::X86_64 => SystemArchitecture::X86_64,
        Platform::Aarch64 => SystemArchitecture::Aarch64,
        Platform::Armv7h => SystemArchitecture::Armv7h,
    }
}

#[cfg(test)]
mod architecture_tests {
    use super::architectures;
    use alpm_types::SystemArchitecture;
    use aurcache_db::lists::Platforms;

    #[test]
    fn each_platform_maps_to_its_alpm_architecture() {
        assert_eq!(
            architectures(&"x86_64;aarch64;armv7h".parse::<Platforms>().unwrap()),
            vec![
                SystemArchitecture::Aarch64,
                SystemArchitecture::Armv7h,
                SystemArchitecture::X86_64,
            ]
        );
    }

    /// A package built only for aarch64 must not be described by x86_64's
    /// dependency list, which is what the hardcoded architecture used to do.
    #[test]
    fn a_single_platform_does_not_pull_in_x86_64() {
        assert_eq!(
            architectures(&"aarch64".parse::<Platforms>().unwrap()),
            vec![SystemArchitecture::Aarch64]
        );
    }
}
