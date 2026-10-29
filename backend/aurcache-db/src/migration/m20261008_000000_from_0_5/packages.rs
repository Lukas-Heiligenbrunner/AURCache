//! Package-level conversions of a 0.5.0 database: one row per package base,
//! one package per file, at most one pending build per package and platform,
//! and the dependency edges 0.5.0 never recorded queued for the server.

use crate::builds;
use crate::files;
use crate::packages;
use crate::settings;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter};
use sea_orm_migration::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use tar::{Archive, Builder};

const ACTIVE_BUILD_STATUS: i32 = 0;
const FAILED_BUILD_STATUS: i32 = 2;
const ENQUEUED_BUILD_STATUS: i32 = 3;
const WAITING_FOR_DEPS_STATUS: i32 = 4;

/// A package as 0.5.0 stored it, as far as these conversions read it.
#[derive(FromQueryResult)]
struct Stored {
    id: i32,
    name: String,
    build_flags: Option<String>,
    source_data: String,
    latest_build: Option<i32>,
}

impl Stored {
    /// The package base an AUR package is built from; `None` for any other
    /// source.
    fn aur_base(&self) -> Option<String> {
        match serde_json::from_str(&self.source_data) {
            Ok(packages::SourceData::Aur { name }) => Some(name),
            _ => None,
        }
    }
}

/// Every package, read by column: an entity would select the columns the
/// code knows today, which a later migration can add to.
async fn stored(db: &impl ConnectionTrait) -> Result<Vec<Stored>, DbErr> {
    Stored::find_by_statement(
        db.get_database_backend().build(
            Query::select()
                .columns(
                    ["id", "name", "build_flags", "source_data", "latest_build"].map(Alias::new),
                )
                .from(Alias::new("packages")),
        ),
    )
    .all(db)
    .await
}

/// Turn every package's build flags, which 0.5.0 handed to paru, into the
/// makepkg flags a build now takes; see
/// [`aurcache_common::build_flags::upgrade_from_paru`].
///
/// A flag no build can take is dropped and named in the log: kept, it would
/// fail every build of the package.
pub(super) async fn normalize_build_flags_in_db(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    for pkg in stored(db).await? {
        let stored = pkg.build_flags.unwrap_or_default();
        let flags: Vec<&str> = stored.split(';').collect();
        let upgraded = aurcache_common::build_flags::upgrade_from_paru(&flags);
        if !upgraded.dropped.is_empty() {
            tracing::warn!(
                "{}: dropped build flags makepkg cannot take: {}",
                pkg.name,
                upgraded.dropped.join(" ")
            );
        }
        let normalized = upgraded.flags.join(";");
        if normalized == stored {
            continue;
        }
        db.execute(
            &Query::update()
                .table(Alias::new("packages"))
                .value(Alias::new("build_flags"), normalized)
                .and_where(Expr::col(Alias::new("id")).eq(pkg.id))
                .to_owned(),
        )
        .await?;
    }
    Ok(())
}

/// Name every AUR package after its package base, and keep one row per base:
/// 0.5.0 could hold one per split package.
pub(super) async fn normalize_package_names_and_merge_duplicates(
    db: &impl ConnectionTrait,
) -> Result<(), DbErr> {
    let aur_packages: Vec<(Stored, String)> = stored(db)
        .await?
        .into_iter()
        .filter_map(|pkg| pkg.aur_base().map(|base| (pkg, base)))
        .collect();

    // First normalize all AUR package names to their canonical pkgbase.
    for (pkg, base) in &aur_packages {
        if base != &pkg.name {
            packages::Entity::update_many()
                .col_expr(packages::Column::Name, Expr::value(base.clone()))
                .filter(packages::Column::Id.eq(pkg.id))
                .exec(db)
                .await?;
        }
    }

    // Group packages by base; keep the row with the most recent build.
    let latest_build: HashMap<i32, Option<i32>> = aur_packages
        .iter()
        .map(|(pkg, _)| (pkg.id, pkg.latest_build))
        .collect();

    let mut by_name: HashMap<&str, Vec<i32>> = HashMap::new();
    for (pkg, base) in &aur_packages {
        by_name.entry(base).or_default().push(pkg.id);
    }

    let mut dup_ids: Vec<i32> = Vec::new();
    for ids in by_name.values_mut() {
        if ids.len() > 1 {
            // Sort by descending latest_build id (higher = more recent, None = never built),
            // then ascending package id as a tiebreaker.
            ids.sort_by(|&a, &b| {
                let ba = latest_build.get(&a).copied().flatten();
                let bb = latest_build.get(&b).copied().flatten();
                bb.cmp(&ba).then(a.cmp(&b))
            });
            dup_ids.extend_from_slice(&ids[1..]);
        }
    }

    // For each duplicate: drop its builds and files (the surviving package already has its own),
    // drop its settings, and repoint dependency links.
    for &dup_id in &dup_ids {
        builds::Entity::delete_many()
            .filter(builds::Column::PkgId.eq(dup_id))
            .exec(db)
            .await?;

        files::Entity::delete_many()
            .filter(files::Column::PackageId.eq(dup_id))
            .exec(db)
            .await?;

        // Drop settings for the duplicate; the surviving package keeps its own.
        settings::Entity::delete_many()
            .filter(settings::Column::PkgId.eq(dup_id))
            .exec(db)
            .await?;
    }

    // Delete the now-orphaned duplicate packages.
    if !dup_ids.is_empty() {
        packages::Entity::delete_many()
            .filter(packages::Column::Id.is_in(dup_ids))
            .exec(db)
            .await?;
    }

    Ok(())
}
/// Give each file the package it belongs to, from the 0.5.0 link table, which
/// then goes: a file belongs to exactly one package.
pub(super) async fn merge_file_links(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let db = manager.get_connection();
    let schema = crate::migration::schema_prefix(manager.get_database_backend());
    db.execute_unprepared(&format!(
        "UPDATE {schema}files
         SET package_id = (SELECT package_id
                           FROM {schema}packages_files
                           WHERE packages_files.file_id = files.id
                           LIMIT 1);"
    ))
    .await?;
    manager
        .drop_table(Table::drop().table(Alias::new("packages_files")).to_owned())
        .await
}

pub(super) async fn mark_duplicate_pending_builds_failed(
    db: &impl ConnectionTrait,
) -> Result<(), DbErr> {
    use sea_orm::QuerySelect;

    // (id, pkg_id, platform, status, start_time)
    type PendingBuildRow = (i32, i32, String, Option<i32>, Option<i64>);

    // Before putting a unique index on active/pending builds, mark any duplicate
    // as failed. Select only the columns this migration needs (via a tuple query)
    // rather than the full `builds` entity, so later columns added to the entity
    // (e.g. worker leasing fields) don't make this historical migration select
    // columns that don't exist yet at this point in the chain.
    let mut pending: Vec<PendingBuildRow> = builds::Entity::find()
        .select_only()
        .column(builds::Column::Id)
        .column(builds::Column::PkgId)
        .column(builds::Column::Platform)
        .column(builds::Column::Status)
        .column(builds::Column::StartTime)
        .filter(builds::Column::Status.is_in([
            Some(ACTIVE_BUILD_STATUS),
            Some(ENQUEUED_BUILD_STATUS),
            Some(WAITING_FOR_DEPS_STATUS),
        ]))
        .into_tuple()
        .all(db)
        .await?;

    // Sort to determine winner per (pkg_id, platform):
    // active builds first, then enqueued, then waiting, then most recent start_time, then highest id.
    pending.sort_by(|a, b| {
        let priority = |s: Option<i32>| match s {
            Some(x) if x == ACTIVE_BUILD_STATUS => 0,
            Some(x) if x == ENQUEUED_BUILD_STATUS => 1,
            Some(x) if x == WAITING_FOR_DEPS_STATUS => 2,
            _ => 3,
        };
        priority(a.3)
            .cmp(&priority(b.3))
            .then(b.4.unwrap_or(0).cmp(&a.4.unwrap_or(0)))
            .then(b.0.cmp(&a.0))
    });

    let mut seen = HashSet::new();
    let to_fail: Vec<i32> = pending
        .into_iter()
        .filter_map(|(id, pkg_id, platform, _status, _start_time)| {
            if seen.insert((pkg_id, platform)) {
                None
            } else {
                Some(id)
            }
        })
        .collect();

    if !to_fail.is_empty() {
        builds::Entity::update_many()
            .col_expr(
                builds::Column::Status,
                Expr::value(Some(FAILED_BUILD_STATUS)),
            )
            .filter(builds::Column::Id.is_in(to_fail))
            .exec(db)
            .await?;
    }

    Ok(())
}

const OLD_PACKAGER: &str = "Unknown Packager";
const NEW_PACKAGER: &str = "AURCache <aurcache@localhost>";

/// Scan every platform directory under `./repo/` and patch `repo.db.tar.gz`
/// and `repo.files.tar.gz` so that `%PACKAGER%` reads
/// `AURCache <aurcache@localhost>` instead of the makepkg default
/// `Unknown Packager`, which libalpm cannot parse.
pub(super) fn patch_repo_packager() -> anyhow::Result<()> {
    let repo_root = std::path::Path::new(aurcache_common::fs::REPO_ROOT);
    if !repo_root.exists() {
        // Nothing to patch? Can happen on the first run.
        return Ok(());
    }

    let read_dir = std::fs::read_dir(repo_root)?;

    for entry in read_dir {
        let platform_dir = entry?.path();
        if !platform_dir.is_dir() {
            continue;
        }

        // The DB for a repo is actually mostly duplicated between
        // two archives that we need to patch.
        for db_name in ["repo.db.tar.gz", "repo.files.tar.gz"] {
            let db_path = platform_dir.join(db_name);
            if !db_path.exists() {
                continue;
            }

            if let Err(err) = patch_db_archive(&db_path) {
                tracing::error!(
                    "Error patching {db_path}: {err:?}.",
                    db_path = db_path.display()
                );
            }
        }
    }

    Ok(())
}

/// Rewrite a single `.db.tar.gz` / `.files.tar.gz`, replacing `Unknown Packager`
/// with `AURCache <aurcache@localhost>` in every `desc` entry, and compressed
/// whatever it held.
///
/// 0.5.0 created the databases of every platform it had not built for as a
/// bare, uncompressed tar under the `.tar.gz` name. pacman reads either, but
/// the repository reads gzip, so the first package published for such a
/// platform -- the first build of a newly added ARM worker -- would fail.
fn patch_db_archive(path: &std::path::Path) -> anyhow::Result<()> {
    let data = std::fs::read(path)?;
    let gzipped = data.starts_with(&[0x1f, 0x8b]);

    // Write to a new file, then move atomically at the end.
    let new_archive_path = path.with_added_extension("new");
    let patched = rewrite_db_archive(&data, gzipped, &new_archive_path);
    match patched {
        Ok(patched) if patched > 0 || !gzipped => {
            std::fs::rename(&new_archive_path, path)?;
        }
        // Nevermind we were never here!
        Ok(_) => std::fs::remove_file(&new_archive_path)?,
        Err(e) => {
            let _ = std::fs::remove_file(&new_archive_path);
            return Err(e);
        }
    }
    Ok(())
}

/// Write `data`, a repository database, to `output` gzipped and with every
/// `Unknown Packager` replaced. Returns the number of entries patched.
fn rewrite_db_archive(
    data: &[u8],
    gzipped: bool,
    output: &std::path::Path,
) -> anyhow::Result<usize> {
    let mut patched = 0usize;

    let reader: Box<dyn Read> = if gzipped {
        Box::new(GzDecoder::new(data))
    } else {
        Box::new(data)
    };
    let mut archive = Archive::new(reader);
    let enc = GzEncoder::new(File::create(output)?, Compression::default());
    let mut builder = Builder::new(enc);

    let unknown_packager = format!("%PACKAGER%\n{OLD_PACKAGER}\n");
    let good_packager = format!("%PACKAGER%\n{NEW_PACKAGER}\n");

    for entry in archive.entries()? {
        let mut entry = entry?;
        let header = entry.header().clone();
        let path_str = header.path()?.to_string_lossy().to_string();

        // Only desc files can contain the %PACKAGER% section.
        if path_str.ends_with("/desc") || path_str == "desc" {
            let mut content = String::new();
            entry.read_to_string(&mut content)?;

            if content.contains(&unknown_packager) {
                content = content.replace(&unknown_packager, &good_packager);
                let bytes = content.as_bytes();
                let mut new_header = header.clone();
                new_header.set_size(bytes.len() as u64);
                new_header.set_cksum();
                builder.append(&new_header, bytes)?;

                patched += 1;
                continue;
            }

            builder.append(&header, content.as_bytes())?;
            continue;
        }

        // Pass through unchanged.
        builder.append(&header, &mut entry)?;
    }

    // Builder::into_inner finishes the archive internally, then we finish the gz stream.
    builder.into_inner()?.finish()?;
    Ok(patched)
}

/// For each existing AUR package that has no rows in the `dependencies` table,
/// query the AUR RPC for its dependencies, insert placeholder package records
/// for any missing AUR deps (recursively), and create the dependency links.
/// Queue the dependency backfill the server runs at its next start: 0.5.0
/// recorded no dependencies, and resolving them takes the server's own
/// resolver and source checkouts, which a migration has neither of.
pub(super) async fn queue_dependency_backfill(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    #[derive(FromQueryResult)]
    struct Count {
        packages: i64,
    }
    let db = manager.get_connection();
    let count = Count::find_by_statement(
        manager.get_database_backend().build(
            Query::select()
                .expr_as(Expr::col(Asterisk).count(), Alias::new("packages"))
                .from(Alias::new("packages")),
        ),
    )
    .one(db)
    .await?
    .map_or(0, |count| count.packages);
    if count == 0 {
        return Ok(());
    }
    manager
        .exec_stmt(
            Query::insert()
                .into_table(Alias::new("operations"))
                .columns([
                    Alias::new("kind"),
                    Alias::new("created_at"),
                    Alias::new("total"),
                ])
                .values_panic([
                    crate::helpers::operations::KIND_DEPENDENCY_BACKFILL.into(),
                    crate::helpers::time::now_secs().into(),
                    <i32 as TryFrom<i64>>::try_from(count)
                        .unwrap_or(i32::MAX)
                        .into(),
                ])
                .to_owned(),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(name: &str, packager: &str) -> String {
        format!("%NAME%\n{name}\n\n%PACKAGER%\n{packager}\n\n")
    }

    /// A tar holding one `desc` per `(name, packager)`.
    fn tar(entries: &[String]) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        for (i, content) in entries.iter().enumerate() {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("pkg{i}-1-1/desc"), content.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut enc, data).unwrap();
        enc.finish().unwrap()
    }

    /// Every `desc` in a gzipped database, in order.
    fn descs(path: &std::path::Path) -> Vec<String> {
        let mut archive = Archive::new(GzDecoder::new(File::open(path).unwrap()));
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut content = String::new();
                entry.unwrap().read_to_string(&mut content).unwrap();
                content
            })
            .collect()
    }

    /// What 0.5.0 left for every platform it never built for: an empty tar,
    /// uncompressed, under the `.tar.gz` name.
    #[test]
    fn an_uncompressed_database_comes_out_gzipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo.db.tar.gz");
        std::fs::write(&path, tar(&[])).unwrap();

        patch_db_archive(&path).unwrap();

        assert!(std::fs::read(&path).unwrap().starts_with(&[0x1f, 0x8b]));
        assert!(descs(&path).is_empty());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "temp file left behind"
        );
    }

    /// An entry that needs no patch keeps its content beside one that does.
    #[test]
    fn unpatched_entries_keep_their_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo.db.tar.gz");
        let kept = desc("kept", "Someone <s@example.com>");
        std::fs::write(
            &path,
            gzip(&tar(&[kept.clone(), desc("old", OLD_PACKAGER)])),
        )
        .unwrap();

        patch_db_archive(&path).unwrap();

        assert_eq!(descs(&path), [kept, desc("old", NEW_PACKAGER)]);
    }

    #[test]
    fn an_unreadable_database_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo.db.tar.gz");
        std::fs::write(&path, [0x1f, 0x8b, 0, 1, 2]).unwrap();

        assert!(patch_db_archive(&path).is_err());

        assert_eq!(std::fs::read(&path).unwrap(), [0x1f, 0x8b, 0, 1, 2]);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "temp file left behind"
        );
    }
}
