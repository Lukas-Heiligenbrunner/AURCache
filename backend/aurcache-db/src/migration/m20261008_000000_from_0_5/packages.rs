//! Package-level conversions of a 0.5.0 database: one row per package base,
//! one package per file, at most one pending build per package and platform,
//! and the dependency edges 0.5.0 never recorded queued for the server.

use crate::builds;
use crate::packages;
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
    /// The name an AUR package was added by -- for 0.5.0, whatever the user
    /// typed, which for a split package is not its base; `None` for any other
    /// source.
    fn aur_name(&self) -> Option<String> {
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

/// Name every package after its package base, and keep one row per name.
///
/// 0.5.0 stored an AUR package under the name it was added by, which for a
/// split package is not its base: the AUR has no repository by that name, so
/// nothing would build it again. The base comes from the package's files in
/// the repository; one never built is renamed at the next start instead, by
/// `aurcache_utils::package::backfill`. Rows that end up with one name -- two
/// names of one split package, a name added twice, a rename into another's --
/// are one package now: the one built most recently stays and takes the
/// others' files, so everything the repository serves keeps an owner.
///
/// `bases` is [`bases_in_repo`]'s answer: the base of each file, by name.
pub(super) async fn normalize_package_names_and_merge_duplicates(
    db: &impl ConnectionTrait,
    bases: &HashMap<String, String>,
) -> Result<(), DbErr> {
    #[derive(FromQueryResult)]
    struct Owned {
        package_id: i32,
        filename: String,
    }
    // Through 0.5.0's link table rather than each file's one owner: a split
    // package's every name built the whole base, so each name links the same
    // files, and only the link table says that all of them are this base.
    let owned = Owned::find_by_statement(
        db.get_database_backend().build(
            Query::select()
                .column((Alias::new("packages_files"), Alias::new("package_id")))
                .column((Alias::new("files"), Alias::new("filename")))
                .from(Alias::new("packages_files"))
                .inner_join(
                    Alias::new("files"),
                    Expr::col((Alias::new("files"), Alias::new("id")))
                        .equals((Alias::new("packages_files"), Alias::new("file_id"))),
                ),
        ),
    )
    .all(db)
    .await?;
    let mut base_of: HashMap<i32, &str> = HashMap::new();
    for file in &owned {
        if let Some(base) = bases.get(&file.filename) {
            base_of.entry(file.package_id).or_insert(base);
        }
    }

    let packages = stored(db).await?;
    let mut by_name: HashMap<String, Vec<&Stored>> = HashMap::new();
    for pkg in &packages {
        let name = match pkg.aur_name() {
            Some(added_as) => base_of
                .get(&pkg.id)
                .map_or_else(|| added_as.trim().to_string(), ToString::to_string),
            None => pkg.name.trim().to_string(),
        };
        by_name.entry(name).or_default().push(pkg);
    }

    for (name, mut rows) in by_name {
        // Most recent build first (a higher id is a later build; never built
        // sorts last), then the oldest row.
        rows.sort_by(|a, b| b.latest_build.cmp(&a.latest_build).then(a.id.cmp(&b.id)));
        let (keep, merged) = rows.split_first().expect("a name groups at least one row");
        for dup in merged {
            tracing::warn!(
                "{name}: package #{} ({}, {}) is the same package as #{} and was merged into it; \
                 its builds and settings are gone, its files now belong to #{}",
                dup.id,
                dup.name,
                dup.source_data,
                keep.id,
                keep.id
            );
            db.execute(
                &Query::update()
                    .table(Alias::new("files"))
                    .value(Alias::new("package_id"), keep.id)
                    .and_where(Expr::col(Alias::new("package_id")).eq(dup.id))
                    .to_owned(),
            )
            .await?;
            for (table, column) in [
                ("builds", "pkg_id"),
                ("settings", "pkg_id"),
                ("packages", "id"),
            ] {
                db.execute(
                    &Query::delete()
                        .from_table(Alias::new(table))
                        .and_where(Expr::col(Alias::new(column)).eq(dup.id))
                        .to_owned(),
                )
                .await?;
            }
        }

        let mut update = Query::update();
        update
            .table(Alias::new("packages"))
            .and_where(Expr::col(Alias::new("id")).eq(keep.id));
        let mut changed = false;
        if keep.name != name {
            tracing::info!("{}: renamed to its package base, {name}", keep.name);
            update.value(Alias::new("name"), name.clone());
            changed = true;
        }
        if keep.aur_name().is_some_and(|added_as| added_as != name) {
            let source = serde_json::to_string(&packages::SourceData::Aur { name: name.clone() })
                .map_err(|e| DbErr::Migration(e.to_string()))?;
            update.value(Alias::new("source_data"), source);
            changed = true;
        }
        if changed {
            db.execute(&update).await?;
        }
    }
    Ok(())
}

/// The package base each file in the repository was built from, by file name.
///
/// Read from the repository databases because they are the only record of
/// it: 0.5.0 stored the name a package was added by, and asking the AUR is
/// not something a migration can count on. A database that cannot be read
/// only leaves its packages to be renamed at the next start.
pub(super) fn bases_in_repo(root: &std::path::Path) -> HashMap<String, String> {
    let mut bases = HashMap::new();
    let Ok(dirs) = std::fs::read_dir(root) else {
        return bases;
    };
    for dir in dirs.flatten() {
        let path = dir.path().join("repo.db.tar.gz");
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        if let Err(e) = read_bases(&data, &mut bases) {
            tracing::warn!("could not read {}: {e:#}", path.display());
        }
    }
    bases
}

/// Add every entry of `data`, a repository database, gzipped or not.
fn read_bases(data: &[u8], bases: &mut HashMap<String, String>) -> anyhow::Result<()> {
    let reader: Box<dyn Read> = if data.starts_with(&[0x1f, 0x8b]) {
        Box::new(GzDecoder::new(data))
    } else {
        Box::new(data)
    };
    for entry in Archive::new(reader).entries()? {
        let mut entry = entry?;
        if !entry.path()?.to_string_lossy().ends_with("/desc") {
            continue;
        }
        let mut desc = String::new();
        entry.read_to_string(&mut desc)?;
        if let (Some(file), Some(base)) = (desc_field(&desc, "FILENAME"), desc_field(&desc, "BASE"))
        {
            bases.insert(file.to_string(), base.to_string());
        }
    }
    Ok(())
}

/// The value of a `desc` field: the line after its `%NAME%` header.
fn desc_field<'a>(desc: &'a str, name: &str) -> Option<&'a str> {
    let header = format!("%{name}%");
    let mut lines = desc.lines();
    lines.find(|line| *line == header)?;
    lines.next()
}

/// Give each file the package it belongs to, from the 0.5.0 link table: a file
/// belongs to exactly one package now. The table stays until
/// [`drop_file_links`], for what still needs every package a file was built by.
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
    Ok(())
}

/// Drop 0.5.0's link table, once nothing reads it.
pub(super) async fn drop_file_links(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
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
