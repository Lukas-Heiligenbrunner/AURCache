use std::collections::HashSet;

use alpm_srcinfo::SourceInfoV1;

use crate::model::{Package, PkgDeps};

/// Extract dependencies and sub-package names from a parsed .SRCINFO, for the
/// architectures the package is actually built for.
///
/// A PKGBUILD may declare `depends_aarch64` separately from `depends`, so the
/// answer genuinely differs per architecture. This used to be hardcoded to
/// x86_64, which meant a package built only for aarch64 had its dependency
/// graph computed from an architecture it is never built on — missing what it
/// needs and requiring what it does not.
///
/// The result is the union across `architectures`: the graph is stored once
/// per package, not once per platform, so it has to cover every platform the
/// package is built for. That over-requires when two architectures need
/// different things, which is the safe direction — a dependency that is built
/// but unused costs a build, where a missing one breaks the package.
pub fn deps_from_srcinfo(
    source_info: &SourceInfoV1,
    architectures: &[alpm_types::SystemArchitecture],
) -> PkgDeps {
    // An empty platform list would otherwise yield no dependencies at all,
    // which reads as "this package needs nothing" rather than as missing
    // configuration.
    let fallback = [alpm_types::SystemArchitecture::X86_64];
    let architectures = if architectures.is_empty() {
        &fallback[..]
    } else {
        architectures
    };

    let packages = architectures
        .iter()
        .flat_map(|arch| source_info.packages_for_architecture(arch.clone()))
        .collect::<Vec<_>>();

    PkgDeps {
        depends: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.dependencies.iter().map(ToString::to_string)),
        ),
        make_depends: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.make_dependencies.iter().map(ToString::to_string)),
        ),
        pkgnames: collect_unique_strings(packages.iter().map(|pkg| pkg.name.to_string())),
        provides: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.provides.iter().map(ToString::to_string)),
        ),
    }
}

/// Split a pacman-style dependency string into `(name, version_constraint)`.
/// e.g. "glibc>=2.35" -> ("glibc", ">=2.35")
/// e.g. "python" -> ("python", "")
///
/// This parser only recognizes the standard pacman comparison operators
/// `>=`, `<=`, `=`, `>`, and `<`.
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

pub(crate) fn deps_from_packages(packages: &[Package]) -> PkgDeps {
    PkgDeps {
        depends: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.depends.iter().flatten().cloned()),
        ),
        make_depends: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.make_depends.iter().flatten().cloned()),
        ),
        pkgnames: collect_unique_strings(packages.iter().map(|pkg| pkg.name.clone())),
        provides: collect_unique_strings(
            packages
                .iter()
                .flat_map(|pkg| pkg.provides.iter().flatten().cloned()),
        ),
    }
}

fn collect_unique_strings<I>(values: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter_map(|value| seen.insert(value.clone()).then_some(value))
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::model::Package;

    use super::{deps_from_packages, parse_dep};

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
    fn deps_from_packages_collects_generic_dependencies() {
        let pkg: Package = serde_json::from_value(serde_json::json!({
            "Name": "parent",
            "Version": "1.0.0",
            "Description": null,
            "Maintainer": null,
            "URL": null,
            "NumVotes": 0,
            "Popularity": 0.0,
            "OutOfDate": null,
            "PackageBase": "parent",
            "PackageBaseID": 0,
            "FirstSubmitted": 0,
            "LastModified": 0,
            "URLPath": null,
            "ID": 0,
            "Depends": ["common-lib"],
            "MakeDepends": ["build-tool"],
            "OptDepends": null,
            "CheckDepends": null,
            "Conflicts": null,
            "Provides": null,
            "Replaces": null,
            "Groups": null,
            "License": null,
            "Keywords": null
        }))
        .unwrap();

        let deps = deps_from_packages(&[pkg]);

        assert_eq!(deps.depends, vec!["common-lib".to_string()]);
        assert_eq!(deps.make_depends, vec!["build-tool".to_string()]);
    }
}

#[cfg(test)]
mod architecture_tests {
    use super::deps_from_srcinfo;
    use alpm_srcinfo::SourceInfoV1;
    use alpm_types::SystemArchitecture;

    /// A PKGBUILD that needs different things on different architectures.
    fn srcinfo() -> SourceInfoV1 {
        let raw = "\
pkgbase = demo
\tpkgdesc = demo package
\tpkgver = 1.0
\tpkgrel = 1
\turl = https://example.com
\tarch = x86_64
\tarch = aarch64
\tdepends = common-lib
\tdepends_x86_64 = intel-only
\tdepends_aarch64 = arm-only
\tmakedepends = build-tool

pkgname = demo
";
        SourceInfoV1::from_string(raw).expect("fixture should parse")
    }

    #[test]
    fn only_the_requested_architectures_dependencies_are_returned() {
        let info = srcinfo();

        let x86 = deps_from_srcinfo(&info, &[SystemArchitecture::X86_64]);
        assert!(x86.depends.contains(&"common-lib".to_string()));
        assert!(x86.depends.contains(&"intel-only".to_string()));
        assert!(
            !x86.depends.contains(&"arm-only".to_string()),
            "x86_64 must not require an aarch64-only dependency: {:?}",
            x86.depends
        );

        // The case the hardcoded x86_64 got wrong: a package built only for
        // aarch64 was described by an architecture it is never built on.
        let arm = deps_from_srcinfo(&info, &[SystemArchitecture::Aarch64]);
        assert!(
            arm.depends.contains(&"arm-only".to_string()),
            "{:?}",
            arm.depends
        );
        assert!(
            !arm.depends.contains(&"intel-only".to_string()),
            "aarch64 must not require an x86_64-only dependency: {:?}",
            arm.depends
        );
    }

    /// The graph is stored once per package, so building for both platforms
    /// has to require what either one needs.
    #[test]
    fn several_architectures_union_their_dependencies() {
        let deps = deps_from_srcinfo(
            &srcinfo(),
            &[SystemArchitecture::X86_64, SystemArchitecture::Aarch64],
        );

        for expected in ["common-lib", "intel-only", "arm-only"] {
            assert!(
                deps.depends.contains(&expected.to_string()),
                "missing {expected}: {:?}",
                deps.depends
            );
        }
        // Shared entries are listed once, not once per architecture.
        assert_eq!(
            deps.depends.iter().filter(|d| *d == "common-lib").count(),
            1,
            "{:?}",
            deps.depends
        );
    }

    /// No configured platform is missing configuration, not a package that
    /// needs nothing — falling through to no dependencies at all would drop a
    /// package's entire graph.
    #[test]
    fn no_architectures_falls_back_rather_than_returning_nothing() {
        let deps = deps_from_srcinfo(&srcinfo(), &[]);
        assert!(
            deps.depends.contains(&"common-lib".to_string()),
            "{:?}",
            deps.depends
        );
    }
}
