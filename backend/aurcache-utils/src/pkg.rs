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

/// Insert a bound into `conjunction`, the bounds on the dependency `name`.
///
/// Same-direction bounds tighten: only the strictest lower and the strictest
/// upper bound survive (a strict operator wins a version tie, so `>1.0`
/// replaces `>=1.0` rather than the other way round). Bounds in opposite
/// directions accumulate — one package declaring `foo>=1.0` and `foo<2.0` is
/// a range, and rejecting it rejects a valid package. A conjunction that is
/// statically empty (`>=2.0` with `<1.0`) still fails loudly: nothing could
/// ever satisfy it, so building anyway would only waste a build.
pub fn merge_bound(
    conjunction: &mut Vec<Constraint>,
    name: &str,
    bound: Constraint,
) -> anyhow::Result<()> {
    use alpm_types::VersionComparison::Equal;

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
    satisfiable(name, conjunction)
}

/// Whether `bound` is a lower (`>`/`>=`) rather than an upper (`<`/`<=`)
/// bound. Exact pins never reach here; see [`merge_bound`].
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

/// Refuse a conjunction that provably admits nothing: its tightest lower bound
/// above its tightest upper one, or equal bounds that exclude the shared
/// version between them (`>1.0` with `<=1.0`). One whose versions cannot be
/// ordered passes. Exact pins never reach here; merge decides those directly.
fn satisfiable(name: &str, conjunction: &[Constraint]) -> anyhow::Result<()> {
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
    let (Some(lower), Some(upper)) = (lower, upper) else {
        return Ok(());
    };
    let empty = match lower.version.partial_cmp(&upper.version) {
        Some(Ordering::Greater) => true,
        Some(Ordering::Equal) => {
            lower.comparison != alpm_types::VersionComparison::GreaterOrEqual
                || upper.comparison != alpm_types::VersionComparison::LessOrEqual
        }
        _ => false,
    };
    anyhow::ensure!(
        !empty,
        "conflicting constraints for '{name}': '{lower}' excludes '{upper}'"
    );
    Ok(())
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
/// The single answer to "what does this pkgbase need?", for the add path and
/// the resync path alike, so that both merge the same way and keep the
/// declared order.
pub struct DependencySet {
    /// Names in declared order, deduplicated.
    ///
    /// The order is load-bearing: it decides the order dependencies are
    /// planned and therefore the order their builds are enqueued. Iterating
    /// `constraints` instead would vary between processes and make identical
    /// input produce different build orders.
    ///
    /// One name can carry several bounds (a range declared as two entries);
    /// see [`merge_bound`].
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
            // An unversioned name has no bound, and is still recorded: the
            // entry is the dependency.
            let conjunction = set.constraints.entry(dep.name.to_string()).or_default();
            if let Some(bound) = parse_dep_constraint(dep.constraint) {
                merge_bound(conjunction, dep.name, bound)?;
            }
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

    /// Merge each of `specs` in turn into one conjunction, joined as stored.
    fn merged(specs: &[&str]) -> anyhow::Result<String> {
        let mut conjunction = Vec::new();
        for spec in specs {
            merge_bound(&mut conjunction, "foo", parse_dep_constraint(spec).unwrap())?;
        }
        Ok(join_constraints(&conjunction))
    }

    #[test]
    fn test_merge_bound_last_wins() {
        assert_eq!(merged(&[">=2.0", ">=3.0"]).unwrap(), ">=3.0");
    }

    /// An unversioned dependency is still a dependency: no bounds must not
    /// mean no entry, or the edge it stands for is never written.
    #[test]
    fn an_unversioned_dependency_is_still_recorded() {
        let set = DependencySet::parse(["mydep".to_string()].iter()).unwrap();
        assert_eq!(set.names, ["mydep"]);
        assert_eq!(set.constraints.get("mydep"), Some(&Vec::new()));
    }

    /// Bounds in opposite directions are a range, not a conflict: one
    /// package declaring both must stay addable.
    #[test]
    fn test_merge_bound_accumulates_a_range() {
        assert_eq!(merged(&[">=3.11", "<3.13"]).unwrap(), ">=3.11,<3.13");
    }

    /// A strict operator wins a version tie: `>1.0` after `>=1.0` tightens,
    /// and `>=1.0` after `>1.0` adds nothing.
    #[test]
    fn test_merge_bound_strict_wins_ties() {
        assert_eq!(merged(&[">=1.0", ">1.0"]).unwrap(), ">1.0");
        assert_eq!(merged(&[">1.0", ">=1.0"]).unwrap(), ">1.0");
    }

    /// A range nothing can satisfy fails at merge time, not three builds
    /// later when no version ever matches.
    #[test]
    fn test_merge_bound_rejects_empty_ranges() {
        assert!(merged(&[">=2.0", "<1.0"]).is_err());
        assert!(merged(&[">1.0", "<=1.0"]).is_err());
        assert_eq!(merged(&[">=1.0", "<=1.0"]).unwrap(), ">=1.0,<=1.0");
    }

    /// An exact pin absorbs the bounds it is consistent with and refuses the
    /// ones it is not.
    #[test]
    fn test_merge_bound_pin() {
        assert_eq!(merged(&[">=1.0", "=1.5"]).unwrap(), "=1.5");
        assert!(merged(&["=1.5", "<1.0"]).is_err());
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
    /// dependency list.
    #[test]
    fn a_single_platform_does_not_pull_in_x86_64() {
        assert_eq!(
            architectures(&"aarch64".parse::<Platforms>().unwrap()),
            vec![SystemArchitecture::Aarch64]
        );
    }
}
