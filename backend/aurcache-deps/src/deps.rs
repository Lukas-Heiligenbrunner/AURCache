use std::collections::HashSet;

use alpm_srcinfo::SourceInfoV1;

use crate::model::{Package, PkgDeps};

/// Extract dependencies and sub-package names from a parsed .SRCINFO.
pub fn deps_from_srcinfo(source_info: &SourceInfoV1) -> PkgDeps {
    let packages = source_info
        .packages_for_architecture(alpm_types::SystemArchitecture::X86_64)
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

    use super::deps_from_packages;

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
