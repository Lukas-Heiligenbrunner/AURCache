//! Build-level conversions of a 0.5.0 database: a status and a number for
//! every build, its log in a file, and what it produced measured.

use aurcache_common::build_state::BuildState;
use aurcache_common::fs::{REPO_ROOT, build_log_path};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use sea_orm_migration::prelude::*;
use std::collections::HashMap;
use std::path::Path;

#[derive(DeriveIden)]
enum Builds {
    Table,
    Id,
    PkgId,
    Status,
    StartTime,
    EndTime,
    Platform,
    Number,
    Size,
    Output,
}

#[derive(DeriveIden)]
enum Files {
    Table,
    Id,
    Filename,
    Platform,
    PackageId,
    Size,
}

/// A build 0.5.0 left without a status ended without saying how: failed.
pub(super) async fn fail_unknown_statuses(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .exec_stmt(
            Query::update()
                .table(Builds::Table)
                .value(Builds::Status, BuildState::Failed.as_i32())
                .and_where(Expr::col(Builds::Status).is_null())
                .to_owned(),
        )
        .await
}

/// Give every build its number within its package, in the order the builds
/// were made.
pub(super) async fn number_builds(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .get_connection()
        .execute_unprepared(
            "UPDATE builds SET number = (
                 SELECT COUNT(*) FROM builds AS earlier
                 WHERE earlier.pkg_id = builds.pkg_id AND earlier.id <= builds.id
             );",
        )
        .await?;
    Ok(())
}

/// Give every build a start time, so lists can order by the bare column: its
/// end, or the epoch.
pub(super) async fn fill_start_times(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .exec_stmt(
            Query::update()
                .table(Builds::Table)
                .value(
                    Builds::StartTime,
                    Expr::FunctionCall(Func::coalesce([Expr::col(Builds::EndTime), Expr::val(0)])),
                )
                .and_where(Expr::col(Builds::StartTime).is_null())
                .to_owned(),
        )
        .await
}

/// Write each build's log out of the `output` column into its file, named
/// after the build as the API names it. A build that logged nothing gets no
/// file.
///
/// One row at a time: logs can be large, and reading every one at once would
/// hold them all in memory.
pub(super) async fn move_logs_to_files(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    #[derive(FromQueryResult)]
    struct Logged {
        id: i32,
        number: i32,
        pkgbase: String,
    }
    let db = manager.get_connection();
    let backend = db.get_database_backend();
    let logged = Logged::find_by_statement(Statement::from_string(
        backend,
        "SELECT b.id AS id, b.number AS number, p.name AS pkgbase \
         FROM builds b JOIN packages p ON p.id = b.pkg_id \
         WHERE b.output IS NOT NULL AND b.output <> ''",
    ))
    .all(db)
    .await?;

    for Logged {
        id,
        number,
        pkgbase,
    } in &logged
    {
        let Some(row) = db
            .query_one(
                &Query::select()
                    .column(Builds::Output)
                    .from(Builds::Table)
                    .and_where(Expr::col(Builds::Id).eq(*id))
                    .to_owned(),
            )
            .await?
        else {
            continue;
        };
        let output: String = row.try_get("", "output")?;
        let path = build_log_path(pkgbase, *number);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DbErr::Migration(format!("cannot create {}: {e}", parent.display()))
            })?;
        }
        std::fs::write(&path, output.as_bytes())
            .map_err(|e| DbErr::Migration(format!("cannot write {}: {e}", path.display())))?;
    }
    tracing::info!("moved {} build log(s) out of the database", logged.len());
    Ok(())
}

/// Record each file's size from the repository, and the total of each
/// package's newest successful build per platform from them.
///
/// Best-effort: a file that is not on disk keeps no size, and a total that
/// would cover one is not recorded -- a partial sum reads as a wrong number.
/// Only the newest successful build can be measured: its artifacts are the
/// ones still in the repository.
pub(super) async fn measure_outputs(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    #[derive(FromQueryResult)]
    struct File {
        id: i32,
        filename: String,
        platform: Option<String>,
        package_id: i32,
    }
    let db = manager.get_connection();
    let backend = db.get_database_backend();
    let files = File::find_by_statement(
        backend.build(
            Query::select()
                .columns([
                    Files::Id,
                    Files::Filename,
                    Files::Platform,
                    Files::PackageId,
                ])
                .from(Files::Table),
        ),
    )
    .all(db)
    .await?;

    let mut totals: HashMap<(i32, Option<String>), Option<i64>> = HashMap::new();
    for file in files {
        let size = file.platform.as_deref().and_then(|platform| {
            let path = Path::new(REPO_ROOT).join(platform).join(&file.filename);
            std::fs::metadata(path)
                .ok()
                .map(|meta| <i64 as TryFrom<u64>>::try_from(meta.len()).unwrap_or(i64::MAX))
        });
        if let Some(size) = size {
            manager
                .exec_stmt(
                    Query::update()
                        .table(Files::Table)
                        .value(Files::Size, size)
                        .and_where(Expr::col(Files::Id).eq(file.id))
                        .to_owned(),
                )
                .await?;
        }
        let total = totals
            .entry((file.package_id, file.platform))
            .or_insert(Some(0));
        *total = total.and_then(|sum| size.map(|size| sum + size));
    }

    for ((pkg_id, platform), total) in totals {
        let (Some(total), Some(platform)) = (total, platform) else {
            continue;
        };
        let newest = Query::select()
            .expr(Expr::col(Builds::Number).max())
            .from(Builds::Table)
            .and_where(Expr::col(Builds::PkgId).eq(pkg_id))
            .and_where(Expr::col(Builds::Platform).eq(platform.as_str()))
            .and_where(Expr::col(Builds::Status).eq(BuildState::Successful.as_i32()))
            .to_owned();
        manager
            .exec_stmt(
                Query::update()
                    .table(Builds::Table)
                    .value(Builds::Size, total)
                    .and_where(Expr::col(Builds::PkgId).eq(pkg_id))
                    .and_where(Expr::col(Builds::Platform).eq(platform.as_str()))
                    .and_where(Expr::col(Builds::Number).in_subquery(newest))
                    .to_owned(),
            )
            .await?;
    }
    Ok(())
}

/// Remove builds and files whose package is gone, which their foreign keys
/// are about to forbid.
pub(super) async fn delete_orphans(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let schema = crate::migration::schema_prefix(manager.get_database_backend());
    manager
        .get_connection()
        .execute_unprepared(&format!(
            "DELETE FROM {schema}builds WHERE pkg_id NOT IN (SELECT id FROM {schema}packages);
             DELETE FROM {schema}files
                 WHERE package_id IS NULL OR package_id NOT IN (SELECT id FROM {schema}packages);"
        ))
        .await?;
    Ok(())
}

/// Derive every package's status from its builds, as everything that changes
/// a build's status does from now on.
pub(super) async fn derive_package_statuses(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    #[derive(FromQueryResult)]
    struct Package {
        id: i32,
    }
    let db = manager.get_connection();
    let ids = Package::find_by_statement(
        db.get_database_backend().build(
            Query::select()
                .column(Alias::new("id"))
                .from(Alias::new("packages")),
        ),
    )
    .all(db)
    .await?;
    for Package { id } in ids {
        crate::helpers::builds::refresh_package_status(db, id).await?;
    }
    Ok(())
}
