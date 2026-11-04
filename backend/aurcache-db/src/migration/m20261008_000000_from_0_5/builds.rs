//! Build-level conversions of a 0.5.0 database: a status and a number for
//! every build, at most one pending per package and platform, its log in a
//! file, what it produced measured, and the package statuses derived from
//! them.

use aurcache_common::build_state::{BuildState, BuildTrigger, EndReason};
use aurcache_common::fs::{REPO_ROOT, build_log_path};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use sea_orm_migration::prelude::*;
use std::collections::{HashMap, HashSet};
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
    Version,
    Trigger,
    EndReason,
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

/// Remove the builds 0.5.0 made for no platform it knew of.
///
/// Its platform settings could be unticked down to none, which it stored as
/// an empty list and then built for anyway, under an empty platform. A build
/// names one platform now, and a row whose platform is none of them cannot be
/// read at all -- this migration's own status pass included.
pub(super) async fn delete_unknown_platforms(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let known = pacman_mirrors::platforms::Platform::ALL.map(|p| p.as_str());
    manager
        .exec_stmt(
            Query::delete()
                .from_table(Builds::Table)
                .cond_where(
                    Condition::any()
                        .add(Expr::col(Builds::Platform).is_null())
                        .add(Expr::col(Builds::Platform).is_not_in(known)),
                )
                .to_owned(),
        )
        .await
}

/// Fail all but one of the builds 0.5.0 left pending for each package and
/// platform, which the unique index on pending builds is about to forbid.
///
/// The one kept is the furthest along -- running, then queued, then waiting
/// -- and then the one started last.
pub(super) async fn fail_duplicate_pending_builds(
    manager: &SchemaManager<'_>,
) -> Result<(), DbErr> {
    #[derive(FromQueryResult)]
    struct Pending {
        id: i32,
        pkg_id: i32,
        platform: String,
        status: i32,
        start_time: Option<i64>,
    }
    const PRECEDENCE: [BuildState; 3] = [
        BuildState::Active,
        BuildState::Enqueued,
        BuildState::WaitingForDeps,
    ];
    let db = manager.get_connection();
    let mut pending = Pending::find_by_statement(
        db.get_database_backend().build(
            Query::select()
                .columns([
                    Builds::Id,
                    Builds::PkgId,
                    Builds::Platform,
                    Builds::Status,
                    Builds::StartTime,
                ])
                .from(Builds::Table)
                .and_where(Expr::col(Builds::Status).is_in(PRECEDENCE.map(BuildState::as_i32))),
        ),
    )
    .all(db)
    .await?;

    let rank = |status: i32| PRECEDENCE.iter().position(|state| state.as_i32() == status);
    pending.sort_by(|a, b| {
        rank(a.status)
            .cmp(&rank(b.status))
            .then(b.start_time.unwrap_or(0).cmp(&a.start_time.unwrap_or(0)))
            .then(b.id.cmp(&a.id))
    });
    let mut seen = HashSet::new();
    let to_fail: Vec<i32> = pending
        .into_iter()
        .filter(|build| !seen.insert((build.pkg_id, build.platform.clone())))
        .map(|build| build.id)
        .collect();
    if to_fail.is_empty() {
        return Ok(());
    }
    manager
        .exec_stmt(
            Query::update()
                .table(Builds::Table)
                .value(Builds::Status, BuildState::Failed.as_i32())
                .and_where(Expr::col(Builds::Id).is_in(to_fail))
                .to_owned(),
        )
        .await
}

/// Hold every build 0.5.0 left pending until its dependencies are recorded.
///
/// 0.5.0 recorded none, so until the backfill this queues has resolved a
/// package, its build reads as ready and a worker would take it before what it
/// depends on -- which 0.5.0 built inside the build. Waiting, each is released
/// as its package is resolved. A build 0.5.0 was running died with it: it
/// fails as interrupted, keeping its log, and a fresh one waits in its place.
pub(super) async fn hold_pending_builds(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let active = || Expr::col(Builds::Status).eq(BuildState::Active.as_i32());
    let now = crate::helpers::time::now_secs();
    manager
        .exec_stmt(
            Query::insert()
                .into_table(Builds::Table)
                .columns([
                    Builds::PkgId,
                    Builds::Platform,
                    Builds::Version,
                    Builds::Status,
                    Builds::StartTime,
                    Builds::Trigger,
                ])
                .select_from(
                    Query::select()
                        .columns([Builds::PkgId, Builds::Platform, Builds::Version])
                        .expr(Expr::val(BuildState::WaitingForDeps.as_i32()))
                        .expr(Expr::val(now))
                        .expr(Expr::val(BuildTrigger::TimeoutRetry.as_i32()))
                        .from(Builds::Table)
                        .and_where(active())
                        .order_by(Builds::Id, Order::Asc)
                        .to_owned(),
                )
                .map_err(|e| DbErr::Migration(e.to_string()))?
                .to_owned(),
        )
        .await?;
    manager
        .exec_stmt(
            Query::update()
                .table(Builds::Table)
                .value(Builds::Status, BuildState::Failed.as_i32())
                .value(Builds::EndReason, EndReason::Interrupted.as_i32())
                .value(
                    Builds::EndTime,
                    Expr::FunctionCall(Func::coalesce([
                        Expr::col(Builds::EndTime),
                        Expr::val(now),
                    ])),
                )
                .and_where(active())
                .to_owned(),
        )
        .await?;
    manager
        .exec_stmt(
            Query::update()
                .table(Builds::Table)
                .value(Builds::Status, BuildState::WaitingForDeps.as_i32())
                .and_where(Expr::col(Builds::Status).eq(BuildState::Enqueued.as_i32()))
                .to_owned(),
        )
        .await
}

/// Give every build its number within its package, in the order the builds
/// were made.
///
/// Each count reads the package's earlier builds through an index made for
/// it: without one, every build scans the whole table, and a large instance
/// sits silent long enough for someone to restart it mid-upgrade.
pub(super) async fn number_builds(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let index = "idx_builds_pkg_id_id";
    manager
        .create_index(
            Index::create()
                .name(index)
                .table(Builds::Table)
                .col(Builds::PkgId)
                .col(Builds::Id)
                .to_owned(),
        )
        .await?;
    manager
        .get_connection()
        .execute_unprepared(
            "UPDATE builds SET number = (
                 SELECT COUNT(*) FROM builds AS earlier
                 WHERE earlier.pkg_id = builds.pkg_id AND earlier.id <= builds.id
             );",
        )
        .await?;
    manager
        .drop_index(Index::drop().name(index).table(Builds::Table).to_owned())
        .await
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
/// a build's status does from now on: from the newest build on each platform
/// the package is built for, the most active first, then a failure, then a
/// success. A package with no build on any of them keeps the status it has.
///
/// Its own copy of `aurcache_db::helpers::builds::refresh_package_status`,
/// reading only the columns it names: that one reads through the entities,
/// which gain every column later versions add.
pub(super) async fn derive_package_statuses(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    #[derive(FromQueryResult)]
    struct Package {
        id: i32,
        platforms: Option<String>,
    }
    #[derive(FromQueryResult)]
    struct Newest {
        pkg_id: i32,
        platform: String,
        status: i32,
    }
    const PRECEDENCE: [BuildState; 6] = [
        BuildState::Active,
        BuildState::Publishing,
        BuildState::Enqueued,
        BuildState::WaitingForDeps,
        BuildState::Failed,
        BuildState::Successful,
    ];
    let db = manager.get_connection();
    let backend = db.get_database_backend();
    let packages = Package::find_by_statement(
        backend.build(
            Query::select()
                .columns([Alias::new("id"), Alias::new("platforms")])
                .from(Alias::new("packages")),
        ),
    )
    .all(db)
    .await?;

    // The newest build per package and platform: the one whose number is
    // the highest of that package and platform.
    let latest = Alias::new("latest");
    let newest_number = Query::select()
        .expr(Expr::col((latest.clone(), Builds::Number)).max())
        .from_as(Builds::Table, latest.clone())
        .and_where(
            Expr::col((latest.clone(), Builds::PkgId)).equals((Builds::Table, Builds::PkgId)),
        )
        .and_where(Expr::col((latest, Builds::Platform)).equals((Builds::Table, Builds::Platform)))
        .to_owned();
    let newest = Newest::find_by_statement(
        backend.build(
            Query::select()
                .columns([Builds::PkgId, Builds::Platform, Builds::Status])
                .from(Builds::Table)
                .and_where(Expr::col((Builds::Table, Builds::Number)).in_subquery(newest_number)),
        ),
    )
    .all(db)
    .await?;
    let mut by_package: HashMap<i32, Vec<Newest>> = HashMap::new();
    for build in newest {
        by_package.entry(build.pkg_id).or_default().push(build);
    }

    let rank = |status: i32| PRECEDENCE.iter().position(|state| state.as_i32() == status);
    for package in packages {
        let Some(builds) = by_package.get(&package.id) else {
            continue;
        };
        let configured: Vec<&str> = package
            .platforms
            .as_deref()
            .unwrap_or_default()
            .split(';')
            .map(str::trim)
            .collect();
        let Some(status) = builds
            .iter()
            .filter(|build| configured.contains(&build.platform.as_str()))
            .map(|build| build.status)
            .min_by_key(|status| rank(*status))
        else {
            continue;
        };
        manager
            .exec_stmt(
                Query::update()
                    .table(Alias::new("packages"))
                    .value(Alias::new("status"), status)
                    .and_where(Expr::col(Alias::new("id")).eq(package.id))
                    .to_owned(),
            )
            .await?;
    }
    Ok(())
}
