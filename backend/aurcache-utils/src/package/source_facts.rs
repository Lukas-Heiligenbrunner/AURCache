//! What a package's source says about it: its dependencies, and the names it
//! is built and provided under.

use crate::snapshot::SnapshotStore;
use alpm_srcinfo::SourceInfoV1;
use anyhow::anyhow;
use aurcache_db::lists::{JsonList, Platforms};
use aurcache_db::packages;
use aurcache_db::packages::SourceData;
use aurcache_deps::PkgDeps;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ConnectionTrait};

/// A package's parsed `.SRCINFO`, and the dependencies it declares.
pub(crate) struct SourceFacts {
    pub(crate) sourceinfo: SourceInfoV1,
    /// For the platforms the package is built for.
    pub(crate) deps: PkgDeps,
}

/// Read `source`, patched by `patch`, and what it depends on when built for
/// `platforms`.
pub(crate) async fn pkg_deps(
    store: &SnapshotStore,
    source: &SourceData,
    patch: Option<&str>,
    platforms: &Platforms,
) -> anyhow::Result<SourceFacts> {
    let sourceinfo = store
        .sourceinfo(source, patch)
        .await
        .map_err(|e| anyhow!("Failed to resolve source info: {e:#}"))?;
    let deps = aurcache_deps::deps_from_srcinfo(&sourceinfo, &crate::pkg::architectures(platforms));
    Ok(SourceFacts { sourceinfo, deps })
}

/// Record on package `pkg_id`'s row the names `deps` says it builds and
/// provides: a build is published only under those, and a dependency finds the
/// package by them.
///
/// `pkgbase` is the row's name, not the `.SRCINFO`'s: the split names are
/// recorded as the ones beyond it (see
/// [`expected_pkgnames`](crate::publish::expected_pkgnames)), and a source can
/// rename its pkgbase under a row that keeps its own.
pub(crate) async fn record_source_names<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    pkgbase: &str,
    deps: &PkgDeps,
) -> anyhow::Result<()> {
    packages::ActiveModel {
        id: Set(pkg_id),
        split_packages: Set(split_packages_json(pkgbase, &deps.pkgnames)),
        provides: Set(provides_json(&deps.provides)),
        ..Default::default()
    }
    .update(db)
    .await?;
    Ok(())
}

/// The `split_packages` column for a package `pkgbase` building `pkgnames`:
/// `None` when it builds only itself.
pub(crate) fn split_packages_json(pkgbase: &str, pkgnames: &[String]) -> Option<JsonList> {
    if pkgnames.len() <= 1 && pkgnames.first().is_none_or(|name| name == pkgbase) {
        return None;
    }
    Some(JsonList::new(pkgnames.iter().cloned()))
}

/// The `provides` column for a package providing `provides`.
pub(crate) fn provides_json(provides: &[String]) -> Option<JsonList> {
    (!provides.is_empty()).then(|| JsonList::new(provides.iter().cloned()))
}

#[cfg(test)]
mod tests {
    use super::split_packages_json;

    /// A source whose pkgbase is not the row's name still records what it
    /// builds: the row's name alone would not admit any of it.
    #[test]
    fn a_renamed_pkgbase_records_its_names() {
        let pkgnames = vec!["bar".to_string()];
        assert_eq!(
            split_packages_json("foo", &pkgnames).map(|names| names.to_string()),
            Some(r#"["bar"]"#.to_string())
        );
        assert_eq!(split_packages_json("bar", &pkgnames), None);
    }
}
