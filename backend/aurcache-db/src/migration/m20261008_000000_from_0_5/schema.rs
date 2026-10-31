//! The schema 0.5.0 lacks, and what it has that goes.

use aurcache_common::build_state::BuildState;
use sea_orm::{ConnectionTrait, DbBackend};
use sea_orm_migration::prelude::*;

/// Tables 0.5.0 does not have, in their final shape.
///
/// `dependencies` gets its indexes later, with the others: the dependency
/// backfill writes to it first.
const SQLITE_TABLES: &str = "
CREATE TABLE api_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL UNIQUE,
    token_hash TEXT NOT NULL UNIQUE
);
CREATE TABLE dependencies (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    dependent_id INTEGER NOT NULL REFERENCES packages (id) ON DELETE CASCADE,
    dependee_id INTEGER NOT NULL REFERENCES packages (id) ON DELETE CASCADE,
    version_constraint TEXT NOT NULL DEFAULT ''
);
CREATE TABLE package_vcs_sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL REFERENCES packages (id) ON DELETE CASCADE,
    source_url TEXT NOT NULL,
    last_commit TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (package_id, source_url)
);
CREATE TABLE workers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    cert_fingerprint TEXT NOT NULL UNIQUE,
    signed_cert TEXT,
    not_after BIGINT,
    native_arches TEXT NOT NULL DEFAULT '',
    emulated_arches TEXT NOT NULL DEFAULT '',
    last_seen BIGINT,
    version TEXT,
    package_affinity TEXT NOT NULL DEFAULT '',
    priority INTEGER NOT NULL DEFAULT 0,
    concurrency INTEGER NOT NULL DEFAULT 1,
    kind TEXT,
    settings_declaration TEXT NOT NULL,
    effective_config TEXT,
    paused BOOLEAN NOT NULL DEFAULT 0
);
CREATE TABLE worker_settings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    worker_id INTEGER NOT NULL REFERENCES workers (id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    UNIQUE (worker_id, key)
);
CREATE TABLE operations (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    finished_at BIGINT,
    total INTEGER NOT NULL,
    completed INTEGER NOT NULL DEFAULT 0,
    failed INTEGER NOT NULL DEFAULT 0,
    log TEXT NOT NULL DEFAULT ''
);
CREATE TABLE log (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    severity INTEGER NOT NULL,
    message TEXT NOT NULL,
    data TEXT NOT NULL,
    scope TEXT,
    timestamp INTEGER NOT NULL,
    user TEXT
);
CREATE TABLE log_entity (
    log_id INTEGER NOT NULL REFERENCES log (id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    ns TEXT NOT NULL,
    id TEXT NOT NULL,
    PRIMARY KEY (log_id, role, ns, id)
);
";

/// [`SQLITE_TABLES`], spelled for Postgres.
const POSTGRES_TABLES: &str = r#"
CREATE TABLE public.api_tokens (
    id SERIAL PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    token_hash TEXT NOT NULL UNIQUE
);
CREATE TABLE public.dependencies (
    id SERIAL PRIMARY KEY,
    dependent_id INTEGER NOT NULL REFERENCES public.packages (id) ON DELETE CASCADE,
    dependee_id INTEGER NOT NULL REFERENCES public.packages (id) ON DELETE CASCADE,
    version_constraint TEXT NOT NULL DEFAULT ''
);
CREATE TABLE public.package_vcs_sources (
    id SERIAL PRIMARY KEY,
    package_id INTEGER NOT NULL REFERENCES public.packages (id) ON DELETE CASCADE,
    source_url TEXT NOT NULL,
    last_commit TEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE (package_id, source_url)
);
CREATE TABLE public.workers (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    cert_fingerprint TEXT NOT NULL UNIQUE,
    signed_cert TEXT,
    not_after BIGINT,
    native_arches TEXT NOT NULL DEFAULT '',
    emulated_arches TEXT NOT NULL DEFAULT '',
    last_seen BIGINT,
    version TEXT,
    package_affinity TEXT NOT NULL DEFAULT '',
    priority INTEGER NOT NULL DEFAULT 0,
    concurrency INTEGER NOT NULL DEFAULT 1,
    kind TEXT,
    settings_declaration TEXT NOT NULL,
    effective_config TEXT,
    paused BOOLEAN NOT NULL DEFAULT false
);
CREATE TABLE public.worker_settings (
    id SERIAL PRIMARY KEY,
    worker_id INTEGER NOT NULL REFERENCES public.workers (id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    UNIQUE (worker_id, key)
);
CREATE TABLE public.operations (
    id SERIAL PRIMARY KEY,
    kind TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    finished_at BIGINT,
    total INTEGER NOT NULL,
    completed INTEGER NOT NULL DEFAULT 0,
    failed INTEGER NOT NULL DEFAULT 0,
    log TEXT NOT NULL DEFAULT ''
);
CREATE TABLE public.log (
    id SERIAL PRIMARY KEY,
    kind TEXT NOT NULL,
    severity INTEGER NOT NULL,
    message TEXT NOT NULL,
    data TEXT NOT NULL,
    scope TEXT,
    "timestamp" BIGINT NOT NULL,
    "user" TEXT
);
CREATE TABLE public.log_entity (
    log_id INTEGER NOT NULL REFERENCES public.log (id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    ns TEXT NOT NULL,
    id TEXT NOT NULL,
    PRIMARY KEY (log_id, role, ns, id)
);
"#;

/// Create the tables 0.5.0 does not have.
pub(super) async fn create_tables(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let sql = match manager.get_database_backend() {
        DbBackend::Sqlite => SQLITE_TABLES,
        DbBackend::Postgres => POSTGRES_TABLES,
        _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
    };
    manager.get_connection().execute_unprepared(sql).await?;
    Ok(())
}

/// Columns 0.5.0's own tables gain, one statement each: SQLite takes a
/// single change per `ALTER TABLE`.
pub(super) async fn add_columns(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let text = |name: &str| ColumnDef::new(Alias::new(name)).text().to_owned();
    let big = |name: &str| ColumnDef::new(Alias::new(name)).big_integer().to_owned();
    let int = |name: &str| ColumnDef::new(Alias::new(name)).integer().to_owned();
    let flag = |name: &str| ColumnDef::new(Alias::new(name)).boolean().to_owned();

    let packages = [
        ColumnDef::new(Alias::new("directly_requested"))
            .boolean()
            .not_null()
            .default(true)
            .to_owned(),
        text("split_packages"),
        text("provides"),
        text("patch"),
        text("source_description"),
        text("source_maintainer"),
        text("source_project_url"),
        text("source_licenses"),
        big("source_first_submitted"),
        big("source_last_modified"),
        flag("aur_flagged_outdated"),
        flag("aur_missing"),
    ];
    // `number` defaults only so it can be added to rows that exist; they are
    // all numbered before anything reads it.
    let builds = [
        int("worker_id"),
        big("lease_expires_at"),
        ColumnDef::new(Alias::new("number"))
            .integer()
            .not_null()
            .default(0)
            .to_owned(),
        big("size"),
        big("peak_memory"),
        ColumnDef::new(Alias::new("trigger"))
            .integer()
            .not_null()
            .default(0)
            .to_owned(),
        int("end_reason"),
        text("vcs_sources"),
        big("disk_chroot"),
        big("disk_workdir"),
        big("disk_sources"),
        big("disk_build_tree"),
        text("kept_path"),
        big("kept_until"),
        text("kept_tree"),
    ];
    // Nullable until every file has its package; see `add_constraints`.
    let files = [int("package_id"), big("size")];

    for (table, columns) in [
        ("packages", &packages[..]),
        ("builds", &builds[..]),
        ("files", &files[..]),
    ] {
        for column in columns {
            manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new(table))
                        .add_column(column.clone())
                        .to_owned(),
                )
                .await?;
        }
    }

    // `out_of_date` has only ever held 0 or 1. SQLite reads either as a
    // boolean already; Postgres needs the type itself changed before anything
    // reads a package.
    if manager.get_database_backend() == DbBackend::Postgres {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.packages ALTER COLUMN out_of_date DROP DEFAULT;
                 ALTER TABLE public.packages ALTER COLUMN out_of_date TYPE BOOLEAN
                     USING out_of_date <> 0;
                 ALTER TABLE public.packages ALTER COLUMN out_of_date SET DEFAULT false;",
            )
            .await?;
    }
    Ok(())
}

/// Drop what 0.5.0 stored and nothing reads any more: the build log column
/// (logs are files now), the package's latest-build pointer (its status is
/// derived from its builds) and its source type (part of `source_data`).
pub(super) async fn drop_columns(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    for (table, column) in [
        ("builds", "output"),
        ("packages", "latest_build"),
        ("packages", "source_type"),
    ] {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new(table))
                    .drop_column(Alias::new(column))
                    .to_owned(),
            )
            .await?;
    }
    Ok(())
}

/// What SQLite cannot add to a table in place, it gets by rebuilding it:
/// `builds` and `files` belong to their package, and a build always has a
/// status.
const SQLITE_CONSTRAINTS: &str = "
CREATE TABLE builds_new (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    pkg_id INTEGER NOT NULL REFERENCES packages (id) ON DELETE CASCADE,
    status INTEGER NOT NULL,
    start_time INTEGER,
    end_time INTEGER,
    platform TEXT,
    version TEXT NOT NULL DEFAULT '',
    worker_id INTEGER,
    lease_expires_at BIGINT,
    number INTEGER NOT NULL DEFAULT 0,
    size BIGINT,
    peak_memory BIGINT,
    trigger INTEGER NOT NULL DEFAULT 0,
    end_reason INTEGER,
    vcs_sources TEXT,
    disk_chroot BIGINT,
    disk_workdir BIGINT,
    disk_sources BIGINT,
    disk_build_tree BIGINT,
    kept_path TEXT,
    kept_until BIGINT,
    kept_tree TEXT
);
INSERT INTO builds_new
    SELECT id, pkg_id, status, start_time, end_time, platform, version, worker_id,
           lease_expires_at, number, size, peak_memory, trigger, end_reason,
           vcs_sources, disk_chroot, disk_workdir, disk_sources, disk_build_tree,
           kept_path, kept_until, kept_tree
    FROM builds;
DROP TABLE builds;
ALTER TABLE builds_new RENAME TO builds;

CREATE TABLE files_new (
    filename TEXT NOT NULL UNIQUE,
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    platform TEXT,
    package_id INTEGER NOT NULL REFERENCES packages (id) ON DELETE CASCADE,
    size BIGINT
);
INSERT INTO files_new (filename, id, platform, package_id, size)
    SELECT filename, id, platform, package_id, size FROM files;
DROP TABLE files;
ALTER TABLE files_new RENAME TO files;
";

/// [`SQLITE_CONSTRAINTS`], which Postgres adds in place.
const POSTGRES_CONSTRAINTS: &str = "
ALTER TABLE public.builds ALTER COLUMN status SET NOT NULL;
ALTER TABLE public.builds ADD CONSTRAINT builds_pkg_id_fkey
    FOREIGN KEY (pkg_id) REFERENCES public.packages (id) ON DELETE CASCADE;
ALTER TABLE public.files ALTER COLUMN package_id SET NOT NULL;
ALTER TABLE public.files ADD CONSTRAINT files_package_id_fkey
    FOREIGN KEY (package_id) REFERENCES public.packages (id) ON DELETE CASCADE;
";

/// Tie builds and files to their package, and make a build's status required.
///
/// The data has to satisfy them first: see `builds::fail_unknown_statuses`
/// and `packages::delete_orphans`.
pub(super) async fn add_constraints(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let sql = match manager.get_database_backend() {
        DbBackend::Sqlite => SQLITE_CONSTRAINTS,
        DbBackend::Postgres => POSTGRES_CONSTRAINTS,
        _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
    };
    manager.get_connection().execute_unprepared(sql).await?;
    Ok(())
}

/// Every index, once the data satisfies the unique ones.
pub(super) async fn create_indexes(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let index = |name: &str, table: &str| {
        Index::create()
            .name(name)
            .table(Alias::new(table))
            .to_owned()
    };
    let col = |name: &str| Alias::new(name);
    let desc = |name: &str| (Alias::new(name), IndexOrder::Desc);

    let indexes = [
        index("idx_packages_name", "packages")
            .col(col("name"))
            .unique()
            .to_owned(),
        index("idx_builds_pkg_number", "builds")
            .col(col("pkg_id"))
            .col(col("number"))
            .unique()
            .to_owned(),
        index("idx_builds_pkg_status_start_time", "builds")
            .col(col("pkg_id"))
            .col(col("status"))
            .col(desc("start_time"))
            .to_owned(),
        index("idx_builds_start_time", "builds")
            .col(desc("start_time"))
            .to_owned(),
        index("idx_builds_status_platform", "builds")
            .col(col("status"))
            .col(col("platform"))
            .to_owned(),
        index("idx_builds_status_start_time", "builds")
            .col(col("status"))
            .col(col("start_time"))
            .to_owned(),
        index("idx_builds_worker_id", "builds")
            .col(col("worker_id"))
            .to_owned(),
        index("idx_files_package_id", "files")
            .col(col("package_id"))
            .to_owned(),
        // One edge per pair, which also serves lookups by `dependent_id`.
        index("idx_dependencies_edge", "dependencies")
            .col(col("dependent_id"))
            .col(col("dependee_id"))
            .unique()
            .to_owned(),
        index("idx_dependencies_dependee", "dependencies")
            .col(col("dependee_id"))
            .to_owned(),
        index("idx_log_timestamp_id", "log")
            .col(desc("timestamp"))
            .col(desc("id"))
            .to_owned(),
        index("idx_log_kind", "log")
            .col(col("kind"))
            .col(desc("timestamp"))
            .col(desc("id"))
            .to_owned(),
        index("idx_log_severity", "log")
            .col(col("severity"))
            .col(desc("timestamp"))
            .col(desc("id"))
            .to_owned(),
        index("idx_log_entity_ref_log", "log_entity")
            .col(col("ns"))
            .col(col("id"))
            .col(col("log_id"))
            .to_owned(),
    ];
    for index in indexes {
        manager.create_index(index).await?;
    }

    // At most one build in progress per package and platform. Partial, which
    // the index builder cannot express.
    let schema = crate::migration::schema_prefix(manager.get_database_backend());
    let in_progress = BuildState::IN_PROGRESS
        .map(|state| state.as_i32().to_string())
        .join(", ");
    manager
        .get_connection()
        .execute_unprepared(&format!(
            "CREATE UNIQUE INDEX idx_builds_pending_pkg_platform ON {schema}builds (pkg_id, platform) \
             WHERE status IN ({in_progress});"
        ))
        .await?;
    Ok(())
}
