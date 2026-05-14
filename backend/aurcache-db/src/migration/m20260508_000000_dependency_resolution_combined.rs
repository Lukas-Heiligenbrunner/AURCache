use crate::dependencies;
use crate::helpers::dbtype::database_type;
use crate::packages;
use aurcache_deps::AurClient;
use sea_orm::DbBackend;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter, Set,
};
use sea_orm_migration::prelude::*;
use std::collections::{HashMap, HashSet};

#[derive(DeriveMigrationName)]
pub struct Migration;

fn old_build_flag_defaults() -> Vec<&'static str> {
    vec![
        "-Syu;--noconfirm;--noprogressbar;--color never",
        "-Byu;--noconfirm;--noprogressbar;--color never",
        "-Byu;--noconfirm;--noprogressbar;--nocolor;--color never",
    ]
}

fn normalize_build_flags(build_flags: &str, new_build_flag_default: &str) -> String {
    if old_build_flag_defaults().contains(&build_flags) {
        return new_build_flag_default.to_string();
    }

    build_flags
        .split(';')
        .map(str::trim)
        .filter(|flag| !flag.is_empty() && *flag != "-Syu" && *flag != "-Byu")
        .collect::<Vec<_>>()
        .join(";")
}

async fn normalize_build_flags_in_db(
    db: &impl ConnectionTrait,
    new_build_flag_default: &str,
) -> Result<(), DbErr> {
    for pkg in packages::Entity::find().all(db).await? {
        let normalized = normalize_build_flags(&pkg.build_flags, new_build_flag_default);
        if normalized == pkg.build_flags {
            continue;
        }

        let mut active: packages::ActiveModel = pkg.into();
        active.build_flags = Set(normalized);
        active.save(db).await?;
    }

    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let new_build_flag_default = "--noconfirm;--noprogressbar;--nocolor;--skippgpcheck";

        match database_type() {
            DbBackend::Sqlite => {
                db.execute_unprepared(
                    r"
alter table packages
    add pkgbase TEXT;

alter table packages
    add directly_requested INTEGER not null default 1;

alter table packages
    add current_version TEXT;

alter table packages
    add split_packages TEXT;

CREATE TABLE IF NOT EXISTS dependencies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    dependent_id INTEGER NOT NULL,
    dependee_id INTEGER NOT NULL,
    platforms TEXT NOT NULL DEFAULT '',
    version_constraint TEXT NOT NULL DEFAULT '',
    FOREIGN KEY (dependent_id) REFERENCES packages(id) ON DELETE CASCADE,
    FOREIGN KEY (dependee_id) REFERENCES packages(id) ON DELETE CASCADE
);
",
                )
                .await?;

                // Backfill NULL pkgbase
                db.execute_unprepared(
                    r"
UPDATE packages
SET pkgbase = json_extract(source_data, '$.name')
WHERE pkgbase IS NULL AND source_type = 'aur';
",
                )
                .await?;
                db.execute_unprepared(
                    r"
UPDATE packages
SET pkgbase = name
WHERE pkgbase IS NULL;
",
                )
                .await?;

                db.execute_unprepared(
                    r"
CREATE UNIQUE INDEX IF NOT EXISTS idx_packages_pkgbase
ON packages (pkgbase);
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
ALTER TABLE public.packages
    ADD pkgbase TEXT;

ALTER TABLE public.packages
    ADD directly_requested BOOLEAN NOT NULL DEFAULT true;

ALTER TABLE public.packages
    ADD current_version TEXT;

ALTER TABLE public.packages
    ADD split_packages TEXT;

CREATE TABLE IF NOT EXISTS public.dependencies (
    id SERIAL PRIMARY KEY,
    dependent_id INTEGER NOT NULL,
    dependee_id INTEGER NOT NULL,
    platforms TEXT NOT NULL DEFAULT '',
    version_constraint TEXT NOT NULL DEFAULT '',
    FOREIGN KEY (dependent_id) REFERENCES public.packages(id) ON DELETE CASCADE,
    FOREIGN KEY (dependee_id) REFERENCES public.packages(id) ON DELETE CASCADE
);
",
                )
                .await?;

                // Backfill NULL pkgbase
                db.execute_unprepared(
                    r"
UPDATE public.packages
SET pkgbase = source_data::json->>'name'
WHERE pkgbase IS NULL AND source_type = 'aur';
",
                )
                .await?;
                db.execute_unprepared(
                    r"
UPDATE public.packages
SET pkgbase = name
WHERE pkgbase IS NULL;
",
                )
                .await?;

                db.execute_unprepared(
                    r"
ALTER TABLE public.packages
ALTER COLUMN pkgbase SET NOT NULL;
",
                )
                .await?;

                db.execute_unprepared(
                    r"
CREATE UNIQUE INDEX IF NOT EXISTS idx_packages_pkgbase
ON public.packages (pkgbase);
",
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        normalize_build_flags_in_db(db, new_build_flag_default).await?;

        tracing::info!("Backfilling dependency entries for existing AUR packages...");
        let client = AurClient::new();
        if let Err(e) = backfill_dependencies(&client, db).await {
            tracing::error!("Dependency backfill failed (non-fatal): {e}");
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        match database_type() {
            DbBackend::Sqlite => {
                db.execute_unprepared("DROP INDEX IF EXISTS idx_packages_pkgbase;")
                    .await?;
                db.execute_unprepared(
                    r"
ALTER TABLE packages DROP COLUMN pkgbase;
ALTER TABLE packages DROP COLUMN directly_requested;
ALTER TABLE packages DROP COLUMN current_version;
ALTER TABLE packages DROP COLUMN split_packages;
DROP TABLE IF EXISTS dependencies;
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    "ALTER TABLE public.packages ALTER COLUMN pkgbase DROP NOT NULL;",
                )
                .await?;
                db.execute_unprepared("DROP INDEX IF EXISTS idx_packages_pkgbase;")
                    .await?;
                db.execute_unprepared(
                    r"
ALTER TABLE public.packages DROP COLUMN pkgbase;
ALTER TABLE public.packages DROP COLUMN directly_requested;
ALTER TABLE public.packages DROP COLUMN current_version;
ALTER TABLE public.packages DROP COLUMN split_packages;
DROP TABLE IF EXISTS public.dependencies;
",
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }
        Ok(())
    }
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

        if let Err(e) = ensure_deps(client, db, &pkg.pkgbase, &mut visited).await {
            tracing::warn!("Failed to process deps for {}: {e}", pkg.pkgbase);
        }
    }

    Ok(())
}

/// Recursively ensure that `pkgbase` and all its AUR dependencies exist in the
/// `packages` table with proper links in the `dependencies` table.
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
        .filter(packages::Column::Pkgbase.eq(pkgbase))
        .one(db)
        .await?
    {
        Some(pkg) => pkg.id,
        None => {
            let new_pkg = packages::ActiveModel {
                name: Set(pkgbase.to_string()),
                pkgbase: Set(pkgbase.to_string()),
                status: Set(3),
                out_of_date: Set(0),
                upstream_version: Set(None),
                latest_build: Set(None),
                build_flags: Set("--noconfirm;--noprogressbar;--nocolor;--skippgpcheck".to_string()),
                platforms: Set("x86_64".to_string()),
                source_type: Set(packages::SourceType::Aur),
                source_data: Set(format!(r#"{{"type":"aur","name":"{pkgbase}"}}"#)),
                directly_requested: Set(false),
                current_version: Set(None),
                split_packages: Set(None),
                ..Default::default()
            };
            let saved = new_pkg.save(db).await.map_err(|e| {
                tracing::warn!("Failed to insert placeholder for {pkgbase}: {e}");
                e
            })?;
            saved.id.clone().unwrap()
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
                .or_insert(constraint.to_string());
            name.to_string()
        })
        .collect();

    if dep_names.is_empty() {
        return Ok(());
    }

    // 5. Batch-resolve which dep names are AUR packages
    let dep_refs: Vec<&str> = dep_names.iter().map(|s| s.as_str()).collect();
    let aur_dep_bases = match client.resolve_bases(&dep_refs).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("resolve_bases failed for {pkgbase}: {e}");
            return Ok(());
        }
    };

    // Build a map from pkgbase → constraint (use the first name that resolved to each base)
    let base_to_constraint: HashMap<&str, &str> = aur_dep_bases
        .iter()
        .map(|(name, base)| {
            (
                base.as_str(),
                dep_constraints
                    .get(name.as_str())
                    .map_or("", |s| s.as_str()),
            )
        })
        .collect();

    // Collect unique AUR pkgbases
    let aur_pkgbases: Vec<&str> = {
        let mut seen = HashSet::new();
        aur_dep_bases
            .values()
            .filter(|b| seen.insert((*b).to_string()))
            .map(|s| s.as_str())
            .collect()
    };

    // 6. Recursively process each AUR dep (this will ensure they exist in DB)
    for dep_base in &aur_pkgbases {
        Box::pin(ensure_deps(client, db, dep_base, visited)).await?;
    }

    // 7. Create dependency links from this package to each resolved AUR dep
    for dep_base in &aur_pkgbases {
        if let Some(dependee) = packages::Entity::find()
            .filter(packages::Column::Pkgbase.eq(*dep_base))
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
                    .get(dep_base)
                    .copied()
                    .unwrap_or("")
                    .to_string();
                dependencies::ActiveModel {
                    dependent_id: Set(pkg_id),
                    dependee_id: Set(dependee.id),
                    platforms: Set("x86_64".to_string()),
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

#[cfg(test)]
mod tests {
    use super::normalize_build_flags;
    use sea_orm::{ConnectionTrait, Database};
    use sea_orm_migration::MigratorTrait;

    use crate::migration::Migrator;

    #[test]
    fn normalize_build_flags_rewrites_legacy_default() {
        assert_eq!(
            normalize_build_flags(
                "-Byu;--noconfirm;--noprogressbar;--color never",
                "--noconfirm;--noprogressbar;--nocolor;--skippgpcheck",
            ),
            "--noconfirm;--noprogressbar;--nocolor;--skippgpcheck"
        );
    }

    #[test]
    fn normalize_build_flags_removes_legacy_tokens_anywhere() {
        assert_eq!(
            normalize_build_flags(
                "--noconfirm;-Byu;--foo;-Syu;--noprogressbar",
                "--noconfirm;--noprogressbar;--nocolor;--skippgpcheck",
            ),
            "--noconfirm;--foo;--noprogressbar"
        );
    }

    #[tokio::test]
    async fn schema_creates_dependencies_table() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        db.execute_unprepared("SELECT * FROM dependencies LIMIT 0")
            .await
            .expect("dependencies table should exist");
    }

    #[tokio::test]
    async fn schema_adds_new_columns_to_packages() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        for col in &[
            "pkgbase",
            "directly_requested",
            "current_version",
            "split_packages",
        ] {
            let sql = format!("SELECT {col} FROM packages LIMIT 0");
            db.execute_unprepared(&sql)
                .await
                .unwrap_or_else(|_| panic!("column '{col}' should exist on packages"));
        }
    }
}
