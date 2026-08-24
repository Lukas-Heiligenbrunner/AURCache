use crate::builds;
use crate::dependencies;
use crate::files;
use crate::helpers::dbtype::database_type;
use crate::helpers::dependency_resolution::resolve_dependency_resolutions;
use crate::packages;
use crate::settings;
use async_recursion::async_recursion;
use aurcache_deps::{AurClient, DependencyResolution};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use sea_orm::DbBackend;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait,
    QueryFilter, Set,
};
use sea_orm_migration::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use tar::{Archive, Builder};

const ACTIVE_BUILD_STATUS: i32 = 0;
const FAILED_BUILD_STATUS: i32 = 2;
const ENQUEUED_BUILD_STATUS: i32 = 3;
const WAITING_FOR_DEPS_STATUS: i32 = 4;

#[derive(DeriveMigrationName)]
pub struct Migration;

fn schema_prefix() -> &'static str {
    if database_type() == DbBackend::Postgres {
        "public."
    } else {
        ""
    }
}

fn normalize_build_flags(build_flags: &str) -> String {
    // Any paru-specific flag should go away.
    build_flags
        .split(';')
        .map(str::trim)
        .filter(|flag| !flag.is_empty() && *flag != "-Syu" && *flag != "-Byu")
        .collect::<Vec<_>>()
        .join(";")
}

async fn normalize_build_flags_in_db(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    for pkg in packages::Entity::find().all(db).await? {
        let normalized = normalize_build_flags(&pkg.build_flags);
        if normalized == pkg.build_flags {
            continue;
        }

        let mut active: packages::ActiveModel = pkg.into();
        active.build_flags = Set(normalized);
        active.save(db).await?;
    }

    Ok(())
}

async fn add_column_if_missing(
    manager: &SchemaManager<'_>,
    table: &str,
    column: &str,
    col_def: ColumnDef,
) -> Result<(), DbErr> {
    if !manager.has_column(table, column).await? {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new(table))
                    .add_column(col_def)
                    .to_owned(),
            )
            .await?;
    }
    Ok(())
}

fn base_name(pkg: &packages::Model) -> String {
    match &pkg.source_data {
        packages::SourceData::Aur { name } => name.clone(),
        _ => pkg.name.clone(),
    }
}

async fn normalize_package_names_and_merge_duplicates(
    db: &impl ConnectionTrait,
) -> Result<(), DbErr> {
    // Until now we could have separate rows for child packages of the same base package.
    // After the migration, rows refer to base packages only, so we should remove duplicates.

    let aur_packages = packages::Entity::find()
        .filter(packages::Column::SourceType.eq(packages::SourceType::Aur))
        .all(db)
        .await?;

    // First normalize all AUR package names to their canonical pkgbase.
    for pkg in &aur_packages {
        let name = base_name(pkg);
        if name != pkg.name {
            packages::Entity::update_many()
                .col_expr(packages::Column::Name, Expr::value(name))
                .filter(packages::Column::Id.eq(pkg.id))
                .exec(db)
                .await?;
        }
    }

    // Group packages by base_name; keep the row with the most recent build.
    let latest_build: HashMap<i32, Option<i32>> = aur_packages
        .iter()
        .map(|p| (p.id, p.latest_build))
        .collect();

    let mut by_name: HashMap<String, Vec<i32>> = HashMap::new();
    for pkg in &aur_packages {
        by_name.entry(base_name(pkg)).or_default().push(pkg.id);
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

    // Clear latest_build on all duplicates in one shot before touching builds rows.
    if !dup_ids.is_empty() {
        packages::Entity::update_many()
            .col_expr(
                packages::Column::LatestBuild,
                Expr::value(sea_orm::Value::Int(None)),
            )
            .filter(packages::Column::Id.is_in(dup_ids.clone()))
            .exec(db)
            .await?;
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
async fn merge_files_package_links(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let db = manager.get_connection();
    let schema = schema_prefix();

    add_column_if_missing(
        manager,
        "files",
        "package_id",
        ColumnDef::new(Alias::new("package_id"))
            .integer()
            .null()
            .to_owned(),
    )
    .await?;

    if manager.has_table("packages_files").await? {
        // Merge the package_files pivot table into the files themselves,
        // since each file now belongs to a single package.
        db.execute_unprepared(&format!(
            "UPDATE {schema}files
             SET package_id = COALESCE(
                 package_id,
                 (SELECT package_id
                  FROM {schema}packages_files
                  WHERE packages_files.file_id = files.id
                  LIMIT 1)
             );"
        ))
        .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(Alias::new("packages_files"))
                    .if_exists()
                    .to_owned(),
            )
            .await?;
    }

    if database_type() == DbBackend::Postgres {
        db.execute_unprepared("ALTER TABLE public.files ALTER COLUMN package_id SET NOT NULL;")
            .await?;
    }

    Ok(())
}

async fn mark_duplicate_pending_builds_failed(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    // Before putting a unique index on active/pending builds, mark any duplicate as failed.
    let mut pending = builds::Entity::find()
        .filter(builds::Column::Status.is_in([
            Some(ACTIVE_BUILD_STATUS),
            Some(ENQUEUED_BUILD_STATUS),
            Some(WAITING_FOR_DEPS_STATUS),
        ]))
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
        priority(a.status)
            .cmp(&priority(b.status))
            .then(b.start_time.unwrap_or(0).cmp(&a.start_time.unwrap_or(0)))
            .then(b.id.cmp(&a.id))
    });

    let mut seen = HashSet::new();
    let to_fail: Vec<i32> = pending
        .into_iter()
        .filter_map(|b| {
            if seen.insert((b.pkg_id, b.platform)) {
                None
            } else {
                Some(b.id)
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

async fn create_pending_build_unique_index(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let db = manager.get_connection();
    let schema = schema_prefix();
    // SeaORM doesn't support partial indexes (indexes with a WHERE clause)
    let index_sql = format!(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_builds_pending_pkg_platform ON {schema}builds (pkg_id, platform) WHERE status IN ({ACTIVE_BUILD_STATUS}, {ENQUEUED_BUILD_STATUS}, {WAITING_FOR_DEPS_STATUS});"
    );
    db.execute_unprepared(&index_sql).await?;
    Ok(())
}

async fn create_dependencies_table(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table(Alias::new("dependencies"))
                .if_not_exists()
                .col(
                    ColumnDef::new(Alias::new("id"))
                        .integer()
                        .not_null()
                        .primary_key()
                        .auto_increment(),
                )
                .col(
                    ColumnDef::new(Alias::new("dependent_id"))
                        .integer()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(Alias::new("dependee_id"))
                        .integer()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(Alias::new("version_constraint"))
                        .text()
                        .not_null()
                        .default(""),
                )
                .foreign_key(
                    ForeignKey::create()
                        .from(Alias::new("dependencies"), Alias::new("dependent_id"))
                        .to(Alias::new("packages"), Alias::new("id"))
                        .on_delete(ForeignKeyAction::Cascade),
                )
                .foreign_key(
                    ForeignKey::create()
                        .from(Alias::new("dependencies"), Alias::new("dependee_id"))
                        .to(Alias::new("packages"), Alias::new("id"))
                        .on_delete(ForeignKeyAction::Cascade),
                )
                .to_owned(),
        )
        .await?;
    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        add_column_if_missing(
            manager,
            "packages",
            "directly_requested",
            ColumnDef::new(Alias::new("directly_requested"))
                .boolean()
                .not_null()
                .default(true)
                .to_owned(),
        )
        .await?;

        create_dependencies_table(manager).await?;

        for col in ["split_packages", "provides"] {
            add_column_if_missing(
                manager,
                "packages",
                col,
                ColumnDef::new(Alias::new(col)).text().null().to_owned(),
            )
            .await?;
        }

        normalize_build_flags_in_db(db).await?;
        merge_files_package_links(manager).await?;
        normalize_package_names_and_merge_duplicates(db).await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_packages_name")
                    .table(Alias::new("packages"))
                    .col(Alias::new("name"))
                    .unique()
                    .if_not_exists()
                    .to_owned(),
            )
            .await
            .map_err(|e| {
                DbErr::Migration(format!(
                    "failed creating unique index idx_packages_name; \
                     likely duplicate package names in packages: {e}"
                ))
            })?;

        mark_duplicate_pending_builds_failed(db).await?;
        create_pending_build_unique_index(manager).await?;

        tracing::info!("Backfilling dependency entries for existing AUR packages...");
        let client = AurClient::new();
        if let Err(e) = backfill_dependencies(&client, db).await {
            tracing::error!("Dependency backfill failed (non-fatal): {e}");
        }

        patch_repo_packager().map_err(|e| DbErr::Custom(e.to_string()))?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(Index::drop().name("idx_packages_name").to_owned())
            .await?;
        manager
            .drop_index(
                Index::drop()
                    .name("idx_builds_pending_pkg_platform")
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("packages"))
                    .drop_column(Alias::new("directly_requested"))
                    .drop_column(Alias::new("split_packages"))
                    .drop_column(Alias::new("provides"))
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("files"))
                    .drop_column(Alias::new("package_id"))
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(Alias::new("dependencies"))
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
}

const OLD_PACKAGER: &str = "Unknown Packager";
const NEW_PACKAGER: &str = "AURCache <aurcache@localhost>";

/// Scan every platform directory under `./repo/` and patch `repo.db.tar.gz`
/// and `repo.files.tar.gz` so that `%PACKAGER%` reads
/// `AURCache <aurcache@localhost>` instead of the makepkg default
/// `Unknown Packager`, which libalpm cannot parse.
fn patch_repo_packager() -> anyhow::Result<()> {
    let repo_root = std::path::Path::new("./repo");
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
/// with `AURCache <aurcache@localhost>` in every `desc` entry.
/// Returns the number of entries patched.
fn patch_db_archive(path: &std::path::Path) -> anyhow::Result<()> {
    let mut patched = 0usize;

    let mut archive = Archive::new(GzDecoder::new(File::open(path)?));

    // Write to a new file, then move atomically at the end.
    let new_archive_path = path.with_added_extension(".new");
    let enc = GzEncoder::new(File::create(&new_archive_path)?, Compression::default());
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
        }

        // Pass through unchanged.
        builder.append(&header, &mut entry)?;
    }

    // Builder::into_inner finishes the archive internally, then we finish the gz stream.
    builder.into_inner()?.finish()?;

    if patched > 0 {
        std::fs::rename(new_archive_path, path)?;
    } else {
        // Nevermind we were never here!
        std::fs::remove_file(new_archive_path)?;
    }

    Ok(())
}

/// For each existing AUR package that has no rows in the `dependencies` table,
/// query the AUR RPC for its dependencies, insert placeholder package records
/// for any missing AUR deps (recursively), and create the dependency links.
pub async fn backfill_dependencies(
    client: &AurClient,
    db: &impl ConnectionTrait,
) -> Result<(), DbErr> {
    let mut visited = HashSet::new();

    let all_pkgs = packages::Entity::find()
        .filter(packages::Column::SourceType.eq(packages::SourceType::Aur))
        .all(db)
        .await?;

    for pkg in &all_pkgs {
        let dep_count = dependencies::Entity::find()
            .filter(dependencies::Column::DependentId.eq(pkg.id))
            .count(db)
            .await?;
        if dep_count > 0 {
            continue;
        }

        if let Err(e) = ensure_deps(client, db, &pkg.name, &mut visited).await {
            tracing::warn!("Failed to process deps for {}: {e}", pkg.name);
        }
    }

    Ok(())
}

/// Recursively ensure that `pkgbase` and all its AUR dependencies exist in the
/// `packages` table with proper links in the `dependencies` table.
#[allow(clippy::double_must_use)]
#[async_recursion]
async fn ensure_deps(
    client: &AurClient,
    db: &impl ConnectionTrait,
    pkgbase: &str,
    visited: &mut HashSet<String>,
) -> Result<(), DbErr> {
    if !visited.insert(pkgbase.to_string()) {
        return Ok(());
    }

    // 1. Ensure the package itself has a row in `packages`
    let pkg_id = match packages::Entity::find()
        .filter(packages::Column::Name.eq(pkgbase))
        .one(db)
        .await?
    {
        Some(pkg) => {
            refresh_package_provides(db, pkg.id, &pkg.name, client).await?;
            pkg.id
        }
        None => {
            let new_pkg = packages::ActiveModel {
                name: Set(pkgbase.to_string()),
                status: Set(3),
                out_of_date: Set(0),
                upstream_version: Set(None),
                latest_build: Set(None),
                build_flags: Set("--noconfirm;--noprogressbar;--nocolor".to_string()),
                platforms: Set("x86_64".to_string()),
                source_type: Set(packages::SourceType::Aur),
                source_data: Set(packages::SourceData::Aur {
                    name: pkgbase.to_string(),
                }),
                directly_requested: Set(false),
                split_packages: Set(None),
                provides: Set(None),
                ..Default::default()
            };
            let saved = new_pkg.save(db).await.map_err(|e| {
                tracing::warn!("Failed to insert placeholder for {pkgbase}: {e}");
                e
            })?;
            let saved_id = match saved.id {
                ActiveValue::Set(id) | ActiveValue::Unchanged(id) => id,
                _ => {
                    return Err(DbErr::Migration(format!(
                        "placeholder package insert for {pkgbase} did not return an id"
                    )));
                }
            };
            refresh_package_provides(db, saved_id, pkgbase, client).await?;
            saved_id
        }
    };

    // 2. If this package already has dependency links, skip further processing
    let dep_count = dependencies::Entity::find()
        .filter(dependencies::Column::DependentId.eq(pkg_id))
        .count(db)
        .await?;
    if dep_count > 0 {
        return Ok(());
    }

    // 3. Fetch dependencies from the AUR RPC
    let deps = match client.deps_of(pkgbase).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("deps_of failed for {pkgbase}: {e}");
            return Ok(());
        }
    };

    // 4. Parse dep names, keeping version constraints
    let mut dep_constraints: HashMap<String, String> = HashMap::new();
    let dep_names: Vec<String> = deps
        .depends
        .iter()
        .chain(deps.make_depends.iter())
        .map(|d| {
            let (name, constraint) = parse_dep(d);
            dep_constraints
                .entry(name.to_string())
                .or_insert_with(|| constraint.to_string());
            name.to_string()
        })
        .collect();

    if dep_names.is_empty() {
        return Ok(());
    }

    // 5. Batch-resolve which dep names are AUR packages
    let resolved_deps = match resolve_dependency_resolutions(client, db, &dep_names).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("dependency resolution failed for {pkgbase}: {e}");
            return Ok(());
        }
    };

    let mut base_to_constraint: HashMap<String, String> = HashMap::new();
    for (name, resolution) in &resolved_deps {
        let base = match resolution {
            DependencyResolution::Official => continue,
            DependencyResolution::Local { pkgbase } | DependencyResolution::Aur { pkgbase } => {
                pkgbase
            }
        };
        if base == pkgbase {
            continue;
        }
        let constraint = dep_constraints
            .get(name.as_str())
            .map_or("", String::as_str);
        base_to_constraint
            .entry(base.clone())
            .or_insert_with(|| constraint.to_string());
    }

    let local_pkgbases: Vec<&str> = {
        let mut seen = HashSet::new();
        resolved_deps
            .values()
            .filter_map(|resolution| match resolution {
                DependencyResolution::Local { pkgbase } => Some(pkgbase.as_str()),
                DependencyResolution::Aur { .. } | DependencyResolution::Official => None,
            })
            .filter(|resolved_pkgbase| *resolved_pkgbase != pkgbase)
            .filter(|pkgbase| seen.insert((*pkgbase).to_string()))
            .collect()
    };

    // Collect unique AUR pkgbases
    let aur_pkgbases: Vec<&str> = {
        let mut seen = HashSet::new();
        resolved_deps
            .values()
            .filter_map(|resolution| match resolution {
                DependencyResolution::Aur { pkgbase } => Some(pkgbase.as_str()),
                DependencyResolution::Official | DependencyResolution::Local { .. } => None,
            })
            .filter(|resolved_pkgbase| *resolved_pkgbase != pkgbase)
            .filter(|b| seen.insert((*b).to_string()))
            .collect()
    };

    // 6. Recursively process each AUR dep (this will ensure they exist in DB)
    for dep_base in &aur_pkgbases {
        ensure_deps(client, db, dep_base, visited).await?;
    }

    // 7. Create dependency links from this package to each resolved local/AUR dep
    for dep_base in local_pkgbases.iter().chain(aur_pkgbases.iter()) {
        if let Some(dependee) = packages::Entity::find()
            .filter(packages::Column::Name.eq(*dep_base))
            .one(db)
            .await?
        {
            let existing = dependencies::Entity::find()
                .filter(dependencies::Column::DependentId.eq(pkg_id))
                .filter(dependencies::Column::DependeeId.eq(dependee.id))
                .one(db)
                .await?;

            if existing.is_none() {
                let constraint = base_to_constraint
                    .get(*dep_base)
                    .cloned()
                    .unwrap_or_default();
                dependencies::ActiveModel {
                    dependent_id: Set(pkg_id),
                    dependee_id: Set(dependee.id),
                    version_constraint: Set(constraint),
                    ..Default::default()
                }
                .save(db)
                .await?;
            }
        }
    }

    Ok(())
}

use aurcache_deps::parse_dep;

async fn refresh_package_provides(
    db: &impl ConnectionTrait,
    pkg_id: i32,
    pkgbase: &str,
    client: &AurClient,
) -> Result<(), DbErr> {
    let provides = match client.deps_of(pkgbase).await {
        Ok(deps) => serialize_optional_list(&deps.provides)?,
        Err(e) => {
            tracing::warn!("deps_of failed for {pkgbase}: {e}");
            return Ok(());
        }
    };

    packages::ActiveModel {
        id: Set(pkg_id),
        provides: Set(provides),
        ..Default::default()
    }
    .update(db)
    .await?;
    Ok(())
}

fn serialize_optional_list(values: &[String]) -> Result<Option<String>, DbErr> {
    if values.is_empty() {
        return Ok(None);
    }

    serde_json::to_string(values)
        .map(Some)
        .map_err(|e| DbErr::Migration(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database};
    use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};

    #[test]
    fn normalize_build_flags_strips_paru_prefix() {
        assert_eq!(
            normalize_build_flags("-Byu;--noconfirm;--noprogressbar;--color never"),
            "--noconfirm;--noprogressbar;--color never"
        );
    }

    #[test]
    fn normalize_build_flags_removes_legacy_tokens_anywhere() {
        assert_eq!(
            normalize_build_flags("--noconfirm;-Byu;--foo;-Syu;--skippgpcheck;--noprogressbar"),
            "--noconfirm;--foo;--skippgpcheck;--noprogressbar"
        );
    }

    #[tokio::test]
    async fn duplicate_pending_builds_are_rejected() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        db.execute_unprepared("INSERT INTO packages (id, name) VALUES (1, 'testpkg');")
            .await
            .unwrap();

        db.execute_unprepared(
            "INSERT INTO builds (id, pkg_id, platform, status) VALUES (1, 1, 'x86_64', 3);",
        )
        .await
        .unwrap();

        let result = db
            .execute_unprepared(
                "INSERT INTO builds (id, pkg_id, platform, status) VALUES (2, 1, 'x86_64', 3);",
            )
            .await;

        assert!(
            result.is_err(),
            "inserting a second pending build for same pkg/platform should fail"
        );

        db.execute_unprepared(
            "INSERT INTO builds (id, pkg_id, platform, status) VALUES (3, 1, 'x86_64', 1);",
        )
        .await
        .expect("inserting a successful build alongside a pending one should succeed");
    }

    #[tokio::test]
    async fn schema_creates_dependencies_table() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        db.execute_unprepared("SELECT * FROM dependencies LIMIT 0")
            .await
            .expect("dependencies table should exist");
        db.execute_unprepared("SELECT version_constraint FROM dependencies LIMIT 0")
            .await
            .expect("version_constraint should exist on dependencies");
        assert!(
            db.execute_unprepared("SELECT platforms FROM dependencies LIMIT 0")
                .await
                .is_err(),
            "dependencies.platforms should not exist"
        );
    }

    #[tokio::test]
    async fn schema_adds_new_columns_to_packages() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        for col in &["directly_requested", "split_packages", "provides"] {
            let sql = format!("SELECT {col} FROM packages LIMIT 0");
            db.execute_unprepared(&sql)
                .await
                .unwrap_or_else(|_| panic!("column '{col}' should exist on packages"));
        }

        db.execute_unprepared("SELECT package_id FROM files LIMIT 0")
            .await
            .expect("column 'package_id' should exist on files");
    }

    #[tokio::test]
    async fn migration_is_idempotent_when_rerun() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await.unwrap();
    }
}
