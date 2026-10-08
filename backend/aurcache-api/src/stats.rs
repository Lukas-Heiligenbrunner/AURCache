use crate::auth::has_api_token;

use rocket::serde::json::Json;

use crate::build::{BuildRow, annotate_waiting, build_row_select};
use crate::models::authenticated::Authenticated;
use crate::models::stats::{
    DashboardView, GraphDataPoint, ListStats, LongBuild, OutOfDateSlice, QueueSlice, UserInfo,
};
use crate::package::package_row_select;
use crate::utils::error::{ApiError, err};
use aurcache_activitylog::log_store::{LogFilter, LogStore};
use aurcache_common::api::activity::Severity;
use aurcache_common::api::builds::BuildSummary;
use aurcache_common::api::log::LogEntry;
use aurcache_common::api::package::SimplePackage;
use aurcache_common::api::stats::{LONGEST_WINDOW_DAYS, RECENT_DAYS};
use aurcache_common::build_state::BuildState;
use aurcache_db::builds;
use aurcache_db::helpers::files::total_artifact_size_expr;
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{packages, workers};
use aurcache_utils::settings;
use rocket::http::Status;
use rocket::{State, get};
use sea_orm::sea_query::{Alias, CaseStatement, Expr, ExprTrait, Func};
use sea_orm::{ColumnTrait, QueryFilter, QuerySelect};
use sea_orm::{DatabaseConnection, EntityTrait};
use sea_orm::{FromQueryResult, JoinType, Order, PaginatorTrait, QueryOrder, RelationTrait};
use std::collections::HashSet;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(stats, dashboard, dashboard_graph_data, user_info))]
pub struct StatsApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Get general build-server stats", body = [ListStats]),
    )
)]
#[get("/stats")]
pub async fn stats(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<ListStats>, ApiError> {
    let db = db.inner();

    get_stats(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))
        .map(Json)
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get infos about the signed in user", body = [UserInfo]),
    )
)]
#[get("/userinfo")]
pub async fn user_info(
    db: &State<DatabaseConnection>,
    a: Authenticated,
) -> Result<Json<UserInfo>, ApiError> {
    let username = a.username;
    let has_api_token = match &username {
        Some(username) => has_api_token(db, username)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?,
        None => false,
    };
    Ok(Json(UserInfo {
        username,
        has_api_token,
    }))
}

#[utoipa::path(
    responses(
        (status = 200, description = "Get graph data for dashboard", body = [Vec<GraphDataPoint>]),
    )
)]
#[get("/graph")]
pub async fn dashboard_graph_data(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<Vec<GraphDataPoint>>, ApiError> {
    let db = db.inner();

    get_graph_datapoints(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))
        .map(Json)
}

/// One month the build graph plots: its calendar name and its first second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GraphMonth {
    year: i32,
    month: i32,
    /// Unix seconds at the start of the month, UTC.
    start: i64,
}

/// The twelve months ending with `now`'s, newest first, in `now`'s zone.
///
/// Computed here rather than in SQL: date arithmetic is the one thing the two
/// backends spell differently, and they did not even agree on it -- SQLite
/// bucketed by UTC month and Postgres by the session's time zone. The server
/// passes its local time, the zone its schedules run in and `/version`
/// reports, so a build at 00:30 on the 1st counts in the month the operator
/// would say it ran in.
///
/// A month starts at the start of its first day in that zone, which jiff
/// resolves where a daylight-saving change skips or repeats midnight.
fn graph_months(now: &jiff::Zoned) -> Vec<GraphMonth> {
    let first = now.date().first_of_month();
    (0_i8..12)
        .filter_map(|back| {
            let day = first.checked_sub(jiff::Span::new().months(back)).ok()?;
            let start = day.to_zoned(now.time_zone().clone()).ok()?;
            Some(GraphMonth {
                year: i32::from(day.year()),
                month: i32::from(day.month()),
                start: start.timestamp().as_second(),
            })
        })
        .collect()
}

/// Builds per month over the last twelve, and how many of them succeeded.
///
/// One grouped query: a `CASE` puts each build in the month whose start it is
/// past, the month boundaries bound as values, so nothing in it is
/// backend-specific. Months with no builds are left out, as they always were.
async fn get_graph_datapoints(db: &DatabaseConnection) -> anyhow::Result<Vec<GraphDataPoint>> {
    #[derive(FromQueryResult)]
    struct MonthRow {
        bucket: i64,
        count: i64,
        successful: i64,
    }

    let months = graph_months(&jiff::Zoned::now());
    let Some(oldest) = months.last() else {
        return Ok(Vec::new());
    };
    let mut bucket = CaseStatement::new();
    for (index, month) in (0_i64..).zip(&months) {
        bucket = bucket.case(Expr::col(builds::Column::StartTime).gte(month.start), index);
    }
    let bucket: Expr = bucket.into();
    let succeeded: Expr = CaseStatement::new()
        .case(
            Expr::col(builds::Column::Status).eq(BuildState::Successful),
            1,
        )
        .finally(0)
        .into();
    // Cast to `BIGINT`: Postgres sums an integer into `numeric` and types a
    // `CASE` of small literals as `int4`, neither of which decodes into an
    // `i64`. SQLite reads the cast as its own INTEGER affinity.
    let rows = Builds::find()
        .select_only()
        .column_as(bucket.cast_as(Alias::new("BIGINT")), "bucket")
        .column_as(
            Expr::col(builds::Column::Id)
                .count()
                .cast_as(Alias::new("BIGINT")),
            "count",
        )
        .column_as(succeeded.sum().cast_as(Alias::new("BIGINT")), "successful")
        .filter(builds::Column::StartTime.gte(oldest.start))
        // By the output column's name rather than by repeating the `CASE`:
        // Postgres binds each boundary as its own parameter, so a second copy
        // of the expression is not "the same expression" to it, and it then
        // refuses the ungrouped `start_time`.
        .group_by(Expr::col(Alias::new("bucket")))
        .into_model::<MonthRow>()
        .all(db)
        .await?;

    let mut points: Vec<GraphDataPoint> = rows
        .into_iter()
        .filter_map(|row| {
            let month = months.get(usize::try_from(row.bucket).ok()?)?;
            Some(GraphDataPoint {
                year: month.year,
                month: month.month,
                count: i32::try_from(row.count).unwrap_or(i32::MAX),
                successful: i32::try_from(row.successful).unwrap_or(i32::MAX),
            })
        })
        .collect();
    points.sort_by_key(|point| std::cmp::Reverse((point.year, point.month)));
    Ok(points)
}

/// Packages someone asked for (`requested`), or ones present only as
/// dependencies of those.
async fn count_packages(db: &DatabaseConnection, requested: bool) -> anyhow::Result<u32> {
    let count = Packages::find()
        .filter(aurcache_db::packages::Column::DirectlyRequested.eq(requested))
        .count(db)
        .await?
        .try_into()?;
    Ok(count)
}

/// SQL counterpart of elapsed_between: unknown for missing, reversed or
/// overflowing endpoints. CASE guards arithmetic that PostgreSQL would reject.
fn build_elapsed_seconds_expr() -> sea_orm::sea_query::SimpleExpr {
    let start = Expr::col(builds::Column::StartTime);
    let end = Expr::col(builds::Column::EndTime);
    let difference = end.clone().sub(start.clone());
    let checked = Expr::case(start.clone().gte(0), difference.clone())
        .case(end.clone().lte(start.clone().add(i64::MAX)), difference)
        .finally(Expr::val(None::<i64>));
    Expr::case(end.gte(start), checked)
        .finally(Expr::val(None::<i64>))
        .into()
}

/// Average duration of a successful build, in seconds.
///
/// Missing averages are unknown; reversed or overflowing intervals are excluded.
async fn avg_build_time(db: &DatabaseConnection) -> anyhow::Result<Option<u64>> {
    let average: Option<Option<f64>> = Builds::find()
        .select_only()
        // Cast because Postgres averages integers into `numeric`.
        .column_as(
            Expr::from(Func::avg(build_elapsed_seconds_expr()))
                .cast_as(Alias::new("DOUBLE PRECISION")),
            "avg_build_time",
        )
        .filter(builds::Column::EndTime.is_not_null())
        .filter(Expr::col(builds::Column::EndTime).gte(Expr::col(builds::Column::StartTime)))
        .filter(builds::Column::Status.eq(BuildState::Successful))
        .into_tuple()
        .one(db)
        .await?;
    Ok(average
        .flatten()
        .filter(|avg| avg.is_finite() && *avg >= 0.0)
        .map(|avg| avg as u64))
}

/// Change over the last 30 days relative to the 30 days before it, as a
/// fraction (`0.5` = up 50%). Each is `0.0` when the earlier window is empty.
struct BuildTrends {
    count: f32,
    duration: f32,
}

/// How many builds started in a window, and how long they took on average.
struct WindowStats {
    count: i64,
    /// `None` when none of them has finished.
    avg_duration: Option<f64>,
}

/// [`WindowStats`] for builds started in `[from, until)`; no `until` is open
/// ended.
///
/// The bounds are computed in Rust and bound as values: `start_time` is written
/// from the server's clock, so the server's clock is the one to measure
/// windows by, and neither backend's `now()` spelling is needed.
async fn window_stats(
    db: &DatabaseConnection,
    from: i64,
    until: Option<i64>,
) -> anyhow::Result<WindowStats> {
    #[derive(FromQueryResult)]
    struct Row {
        count: i64,
        avg_duration: Option<f64>,
    }
    let mut query = Builds::find()
        .select_only()
        .column_as(
            Expr::col(builds::Column::Id)
                .count()
                .cast_as(Alias::new("BIGINT")),
            "count",
        )
        // A build that has not ended has a NULL difference, which `AVG`
        // leaves out. Cast because Postgres averages integers into `numeric`.
        .column_as(
            Expr::from(Func::avg(build_elapsed_seconds_expr()))
                .cast_as(Alias::new("DOUBLE PRECISION")),
            "avg_duration",
        )
        .filter(builds::Column::StartTime.gte(from));
    if let Some(until) = until {
        query = query.filter(builds::Column::StartTime.lt(until));
    }
    let row = query
        .into_model::<Row>()
        .one(db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("an aggregate without GROUP BY returned no row"))?;
    Ok(WindowStats {
        count: row.count,
        avg_duration: row.avg_duration,
    })
}

/// The last 30 days against the 30 before them.
async fn build_trends(db: &DatabaseConnection) -> anyhow::Result<BuildTrends> {
    const WINDOW: i64 = 30 * 24 * 60 * 60;
    let now = aurcache_db::helpers::time::now_secs();
    let (last, prev) = tokio::join!(
        window_stats(db, now - WINDOW, None),
        window_stats(db, now - 2 * WINDOW, Some(now - WINDOW)),
    );
    let (last, prev) = (last?, prev?);
    Ok(BuildTrends {
        count: change(last.count as f64, prev.count as f64),
        duration: change(
            last.avg_duration.unwrap_or(0.0),
            prev.avg_duration.unwrap_or(0.0),
        ),
    })
}

/// `now` relative to `before`, as a fraction: `0.5` is up 50%. `0.0` when
/// there is nothing to compare against.
fn change(now: f64, before: f64) -> f32 {
    if before == 0.0 {
        0.0
    } else {
        (now / before - 1.0) as f32
    }
}

/// Count builds, optionally in one state and optionally only recent ones.
///
/// `since` is a Unix second; builds with no start time are excluded from a
/// windowed count, since an unstarted build has not happened yet.
async fn count_builds(
    db: &DatabaseConnection,
    status: Option<BuildState>,
    since: Option<i64>,
) -> anyhow::Result<u32> {
    let mut query = Builds::find();
    if let Some(status) = status {
        query = query.filter(builds::Column::Status.eq(status));
    }
    if let Some(since) = since {
        query = query.filter(builds::Column::StartTime.gte(since));
    }
    Ok(query.count(db).await?.try_into()?)
}

async fn get_stats(db: &DatabaseConnection) -> anyhow::Result<ListStats> {
    let now = aurcache_db::helpers::time::now_secs();
    let cutoff = now - RECENT_DAYS * 24 * 60 * 60;

    // Independent reads over one pooled connection: serial awaits paid each
    // round trip in turn for queries that share nothing.
    let (
        total_builds,
        successful_builds,
        failed_builds,
        recent_builds,
        recent_successful,
        recent_failed,
        trends,
        avg_build_time,
        requested_packages,
        dependency_packages,
    ) = tokio::join!(
        count_builds(db, None, None),
        count_builds(db, Some(BuildState::Successful), None),
        count_builds(db, Some(BuildState::Failed), None),
        count_builds(db, None, Some(cutoff)),
        count_builds(db, Some(BuildState::Successful), Some(cutoff)),
        count_builds(db, Some(BuildState::Failed), Some(cutoff)),
        build_trends(db),
        avg_build_time(db),
        count_packages(db, true),
        count_packages(db, false),
    );

    let trends = trends?;
    let repo_size = aurcache_db::helpers::files::total_size(db).await?;
    Ok(ListStats {
        total_builds: total_builds?,
        successful_builds: successful_builds?,
        failed_builds: failed_builds?,

        recent_builds: recent_builds?,
        recent_successful: recent_successful?,
        recent_failed: recent_failed?,

        avg_build_time: avg_build_time?,
        repo_size,
        requested_packages: requested_packages?,
        dependency_packages: dependency_packages?,
        total_build_trend: trends.count,
        avg_build_time_trend: trends.duration,
    })
}

/// How many rows each dashboard card shows.
const DASHBOARD_LIMIT: u64 = 5;

#[utoipa::path(
    responses(
            (status = 200, description = "Dashboard slices for the landing page", body = DashboardView),
    )
)]
#[get("/stats/dashboard")]
pub async fn dashboard(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<DashboardView>, ApiError> {
    let db = db.inner();

    fn opt<T>(result: anyhow::Result<T>, section: &str) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(e) => {
                tracing::warn!("dashboard section {section} failed: {e:#}");
                None
            }
        }
    }

    let (recent_packages, recent_builds, failed, out_of_date, queue, problems, largest, longest) = tokio::join!(
        recent_packages(db),
        recent_builds(db),
        failed_packages(db),
        out_of_date_slice(db),
        queue_slice(db),
        problem_entries(db),
        largest_packages(db),
        longest_builds(db),
    );

    Ok(Json(DashboardView {
        recent_packages: opt(recent_packages, "recent_packages"),
        recent_builds: opt(recent_builds, "recent_builds"),
        failed: opt(failed, "failed"),
        out_of_date: opt(out_of_date, "out_of_date"),
        queue: opt(queue, "queue"),
        problems: opt(problems, "problems"),
        largest: opt(largest, "largest"),
        longest: opt(longest, "longest"),
    }))
}

async fn recent_packages(db: &DatabaseConnection) -> anyhow::Result<Vec<SimplePackage>> {
    Ok(package_row_select()
        .filter(packages::Column::DirectlyRequested.eq(true))
        .order_by(packages::Column::Id, Order::Desc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<SimplePackage>()
        .all(db)
        .await?)
}

async fn recent_builds(db: &DatabaseConnection) -> anyhow::Result<Vec<BuildSummary>> {
    let rows = build_row_select()
        .order_by(builds::Column::StartTime, Order::Desc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<BuildRow>()
        .all(db)
        .await?;
    Ok(annotate_waiting(db, rows).await)
}

async fn failed_packages(db: &DatabaseConnection) -> anyhow::Result<Vec<SimplePackage>> {
    Ok(package_row_select()
        .filter(packages::Column::Status.eq(BuildState::Failed))
        .order_by(packages::Column::Id, Order::Desc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<SimplePackage>()
        .all(db)
        .await?)
}

async fn out_of_date_slice(db: &DatabaseConnection) -> anyhow::Result<OutOfDateSlice> {
    // Matches what the rest of the UI calls "out of date": a package whose
    // last build succeeded but that upstream has moved past
    // (`StatusFilter::matches` on the frontend). `out_of_date` is only ever
    // cleared on a successful build (`publish.rs`), not
    // on a failed one, so a package whose rebuild attempt just failed can
    // still carry the flag -- that package belongs on the Failed card, whose
    // "View all" this card's link would otherwise fail to reproduce.
    let outdated: Vec<SimplePackage> = package_row_select()
        .filter(packages::Column::OutOfDate.eq(true))
        .filter(packages::Column::Status.eq(BuildState::Successful))
        .order_by(packages::Column::Id, Order::Desc)
        .into_model::<SimplePackage>()
        .all(db)
        .await?;
    if outdated.is_empty() {
        return Ok(OutOfDateSlice {
            needs_hand: Vec::new(),
            handled: 0,
        });
    }
    let names: Vec<&str> = outdated.iter().map(|pkg| pkg.name.as_str()).collect();

    // Both auto-rebuild paths -- the version checker's immediate rebuild and
    // the scheduled auto-update job -- call `package_update_all_outdated`,
    // which reads these two settings globally, never per package, and skips
    // any package whose last build did not succeed (it "stays flagged for a
    // human", `update.rs`). A per-package override is never consulted by
    // either job, so resolving it here would disagree with what the server
    // actually does.
    let (build_on_new_version, auto_update_schedule) = tokio::join!(
        settings::get(db, settings::key::BUILD_ON_NEW_VERSION, None),
        settings::get(db, settings::key::AUTO_UPDATE_SCHEDULE, None),
    );
    // Whether the cron string itself parses is not re-checked here: an
    // invalid one already surfaces as a `ScheduleInvalid` warning, which the
    // Recent problems card shows.
    let auto_rebuild_configured = build_on_new_version.value
        || auto_update_schedule
            .value
            .as_deref()
            .is_some_and(|expr| !aurcache_common::schedule::is_off(expr));

    // By name, which is unique and all the rows carry.
    let active: HashSet<String> = Builds::find()
        .select_only()
        .column(packages::Column::Name)
        .join(JoinType::InnerJoin, builds::Relation::Packages.def())
        .filter(packages::Column::Name.is_in(names))
        .filter(builds::Column::Status.is_in(BuildState::IN_PROGRESS))
        .into_tuple::<String>()
        .all(db)
        .await?
        .into_iter()
        .collect();

    let mut needs_hand = Vec::new();
    let mut handled = 0u64;
    for pkg in outdated {
        // Every row here already has a successful last build (the query
        // above), so whether either auto-rebuild job will pick it up comes
        // down to the settings alone -- unless one already has, which the
        // active-build check catches.
        if !auto_rebuild_configured && !active.contains(&pkg.name) {
            if needs_hand.len() < DASHBOARD_LIMIT as usize {
                needs_hand.push(pkg);
            }
        } else {
            handled += 1;
        }
    }
    Ok(OutOfDateSlice {
        needs_hand,
        handled,
    })
}

async fn queue_slice(db: &DatabaseConnection) -> anyhow::Result<QueueSlice> {
    let queued = [BuildState::Enqueued, BuildState::WaitingForDeps];
    let depth: u64 = Builds::find()
        .filter(builds::Column::Status.is_in(queued))
        .count(db)
        .await?;
    let rows = build_row_select()
        .filter(builds::Column::Status.is_in(queued))
        .order_by(builds::Column::StartTime, Order::Asc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<BuildRow>()
        .all(db)
        .await?;
    Ok(QueueSlice {
        depth,
        oldest: annotate_waiting(db, rows).await,
    })
}

async fn problem_entries(db: &DatabaseConnection) -> anyhow::Result<Vec<LogEntry>> {
    let page = LogStore::new(db.clone())
        .page(
            DASHBOARD_LIMIT,
            0,
            &LogFilter {
                severity: Some(Severity::Warning),
                ..Default::default()
            },
        )
        .await?;
    Ok(page.entries)
}

async fn largest_packages(db: &DatabaseConnection) -> anyhow::Result<Vec<SimplePackage>> {
    // `total_artifact_size_expr()` is NULL for a package with no files or an
    // unknown size -- "nothing recorded", not "zero bytes" (see the size
    // convention in the crate docs). Excluded rather than sorted: SQLite puts
    // NULL last in `DESC`, Postgres puts it first, so leaving it in would fill
    // this card with unbuilt packages ahead of the real largest ones on one
    // of the two backends.
    Ok(package_row_select()
        .filter(total_artifact_size_expr().is_not_null())
        .order_by(total_artifact_size_expr(), Order::Desc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<SimplePackage>()
        .all(db)
        .await?)
}

/// One long-build candidate with the package id for the previous-run lookup.
#[derive(FromQueryResult)]
struct LongestRow {
    pkg_id: i32,
    number: i32,
    pkg_name: String,
    version: String,
    status: BuildState,
    start_time: Option<i64>,
    end_time: Option<i64>,
    platform: String,
    size: Option<i64>,
    peak_memory: Option<i64>,
    worker_name: Option<String>,
}

async fn longest_builds(db: &DatabaseConnection) -> anyhow::Result<Vec<LongBuild>> {
    let since = aurcache_db::helpers::time::now_secs() - LONGEST_WINDOW_DAYS * 24 * 60 * 60;
    let duration = build_elapsed_seconds_expr();
    let rows: Vec<LongestRow> = Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .select_only()
        .column_as(builds::Column::PkgId, "pkg_id")
        .column_as(builds::Column::Number, "number")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column(builds::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .column(builds::Column::Platform)
        .column(builds::Column::Size)
        .column(builds::Column::PeakMemory)
        .join(JoinType::LeftJoin, builds::Relation::Workers.def())
        .column_as(workers::Column::Name, "worker_name")
        .filter(builds::Column::Status.eq(BuildState::Successful))
        .filter(builds::Column::StartTime.gte(since))
        .filter(builds::Column::EndTime.is_not_null())
        .filter(duration.clone().is_not_null())
        .order_by(duration, Order::Desc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<LongestRow>()
        .all(db)
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let previous_secs = match row.start_time {
            Some(started) => {
                let prev: Option<(Option<i64>, Option<i64>)> = Builds::find()
                    .select_only()
                    .column(builds::Column::StartTime)
                    .column(builds::Column::EndTime)
                    .filter(builds::Column::PkgId.eq(row.pkg_id))
                    .filter(builds::Column::Platform.eq(row.platform.as_str()))
                    .filter(builds::Column::Status.eq(BuildState::Successful))
                    .filter(builds::Column::StartTime.lt(started))
                    .order_by(builds::Column::StartTime, Order::Desc)
                    .limit(1)
                    .into_tuple::<(Option<i64>, Option<i64>)>()
                    .one(db)
                    .await?;
                prev.and_then(|(start, end)| {
                    aurcache_common::api::builds::elapsed_between(start, end)
                })
                .map(|duration| duration.as_secs())
            }
            None => None,
        };
        out.push(LongBuild {
            build: BuildSummary {
                number: row.number,
                pkg_name: row.pkg_name,
                version: row.version,
                status: row.status,
                start_time: row.start_time,
                end_time: row.end_time,
                platform: row.platform,
                size: row.size,
                peak_memory: row.peak_memory,
                disk_usage: None,
                kept: None,
                worker_name: row.worker_name,
                log_size: None,
                waiting_reason: None,
            },
            previous_secs,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{
        GraphMonth, avg_build_time, build_trends, count_packages, failed_packages,
        get_graph_datapoints, graph_months, largest_packages, longest_builds, out_of_date_slice,
        problem_entries, queue_slice, recent_builds, recent_packages,
    };
    use aurcache_common::api::activity::Severity;
    use aurcache_common::build_state::BuildState;
    use aurcache_common::settings::Setting;
    use aurcache_db::helpers::time::now_secs;
    use aurcache_db::migration::Migrator;
    use aurcache_db::packages::{self, SourceData};
    use aurcache_db::{builds, files, logs};
    use aurcache_utils::settings;
    use pacman_mirrors::platforms::Platform;
    use sea_orm::{ActiveModelTrait, Database, EntityTrait, Set};
    use sea_orm_migration::MigratorTrait;

    async fn insert_package(
        db: &sea_orm::DatabaseConnection,
        name: &str,
        out_of_date: bool,
        directly_requested: bool,
    ) -> i32 {
        packages::ActiveModel {
            name: Set(name.to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(out_of_date),
            upstream_version: Set(Some("1.0-1".to_string())),
            build_flags: Set(Default::default()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: name.to_string(),
            }),
            directly_requested: Set(directly_requested),
            split_packages: Set(None),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert package")
        .id
    }

    async fn insert_build(
        db: &sea_orm::DatabaseConnection,
        pkg_id: i32,
        number: i32,
        status: BuildState,
        start: Option<i64>,
        end: Option<i64>,
    ) {
        aurcache_db::prelude::Builds::insert(builds::ActiveModel {
            pkg_id: Set(pkg_id),
            number: Set(number),
            status: Set(status),
            platform: Set(Platform::X86_64),
            version: Set("1.0-1".to_string()),
            start_time: Set(start),
            end_time: Set(end),
            ..Default::default()
        })
        .exec(db)
        .await
        .expect("insert build");
    }

    /// The average covers finished successful builds only, and nothing to
    /// average reads as 0 rather than an error.
    #[tokio::test]
    async fn the_average_build_time_is_over_successful_builds() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        assert_eq!(avg_build_time(&db).await.unwrap(), None);

        let pkg = insert_package(&db, "hello", false, true).await;
        insert_build(&db, pkg, 1, BuildState::Successful, Some(0), Some(100)).await;
        insert_build(&db, pkg, 2, BuildState::Successful, Some(0), Some(200)).await;
        insert_build(&db, pkg, 3, BuildState::Failed, Some(0), Some(9000)).await;
        insert_build(&db, pkg, 4, BuildState::Successful, Some(0), None).await;
        insert_build(&db, pkg, 5, BuildState::Successful, Some(20), Some(10)).await;
        insert_build(&db, pkg, 6, BuildState::Successful, Some(i64::MIN), Some(0)).await;
        insert_build(
            &db,
            pkg,
            7,
            BuildState::Successful,
            Some(i64::MIN),
            Some(i64::MAX),
        )
        .await;
        assert_eq!(avg_build_time(&db).await.unwrap(), Some(150));
        builds::Entity::delete_many().exec(&db).await.unwrap();
        insert_build(&db, pkg, 8, BuildState::Successful, Some(0), Some(0)).await;
        assert_eq!(avg_build_time(&db).await.unwrap(), Some(0));
    }

    /// Run against an empty disposable PostgreSQL database, supplied through
    /// AURCACHE_TEST_POSTGRES_URL. Exercises the backend-specific arithmetic,
    /// locking and journal SQL that SQLite tests cannot check.
    #[tokio::test]
    #[ignore = "requires an empty disposable PostgreSQL database"]
    async fn postgres_elapsed_queue_and_repository_journal() {
        use aurcache_common::build_state::BuildTrigger;
        use aurcache_db::helpers::build_enqueue::{
            Pending, demote_enqueued_build, enqueue_build_if_missing, promote_waiting_build,
        };
        use sea_orm::ConnectionTrait;

        let url = std::env::var("AURCACHE_TEST_POSTGRES_URL").expect("test PostgreSQL URL");
        let db = Database::connect(url).await.unwrap();
        assert_eq!(db.get_database_backend(), sea_orm::DbBackend::Postgres);
        Migrator::up(&db, None).await.unwrap();
        assert_eq!(avg_build_time(&db).await.unwrap(), None);
        let pkg = insert_package(&db, "postgres-review", false, true).await;
        for (number, start, end) in [
            (1, 0, 100),
            (2, 20, 10),
            (3, i64::MIN, 0),
            (4, i64::MIN, i64::MAX),
            (5, 0, 0),
            (6, 100, 50),
        ] {
            insert_build(
                &db,
                pkg,
                number,
                BuildState::Successful,
                Some(start),
                Some(end),
            )
            .await;
        }
        assert_eq!(avg_build_time(&db).await.unwrap(), Some(50));
        assert_eq!(
            super::window_stats(&db, i64::MIN, None)
                .await
                .unwrap()
                .avg_duration,
            Some(50.0)
        );

        let pending = enqueue_build_if_missing(
            &db,
            pkg,
            Platform::X86_64,
            "1",
            0,
            Pending::Enqueued,
            BuildTrigger::User,
        )
        .await
        .unwrap();
        demote_enqueued_build(&db, pkg, Platform::X86_64)
            .await
            .unwrap();
        promote_waiting_build(&db, pkg, Platform::X86_64)
            .await
            .unwrap();
        assert_eq!(
            aurcache_db::prelude::Builds::find_by_id(pending.build.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .status,
            BuildState::Enqueued
        );

        let tmp = tempfile::tempdir().unwrap();
        let repo = aurcache_utils::repository::Repository::new(tmp.path());
        repo.begin(&db)
            .await
            .unwrap()
            .commit(&db, |_| Box::pin(async { Ok(()) }))
            .await
            .unwrap();
        db.execute_unprepared(
            r#"INSERT INTO repository_updates (id, plan) VALUES (1, '{"moves":[],"retired":[]}')"#,
        )
        .await
        .unwrap();
        repo.recover(&db).await.unwrap();
        repo.begin(&db)
            .await
            .unwrap()
            .commit(&db, |_| Box::pin(async { Ok(()) }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stats_only_count_directly_requested_packages() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        packages::ActiveModel {
            name: Set("top-level".to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: "top-level".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        packages::ActiveModel {
            name: Set("transitive-dependency".to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            upstream_version: Set(Some("1.0.0".to_string())),
            build_flags: Set("--noconfirm".parse().unwrap()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: "transitive-dependency".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        // One of each was inserted above, so the two counts partition the
        // table — a filter that ignored the flag would give 2 for both.
        assert_eq!(count_packages(&db, true).await.unwrap(), 1);
        assert_eq!(count_packages(&db, false).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn recent_packages_are_newest_requested_regardless_of_out_of_date() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        insert_package(&db, "pkg-a", false, true).await;
        insert_package(&db, "pkg-b", true, true).await;
        insert_package(&db, "pkg-dep", false, false).await;

        let recent = recent_packages(&db).await.unwrap();
        assert_eq!(recent.len(), 2);
        // Id DESC: pkg-b was inserted after pkg-a, out-of-date or not.
        assert_eq!(recent[0].name, "pkg-b");
        assert_eq!(recent[1].name, "pkg-a");
    }

    #[tokio::test]
    async fn queue_is_oldest_first_depth_counts_past_cap_and_reason_survives() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        // One queued build per package: the pending (pkg_id, platform)
        // unique index forbids two queued builds for the same package on
        // the same platform.
        for number in 1..=7 {
            let pkg_id = insert_package(&db, &format!("queued-pkg-{number}"), false, true).await;
            insert_build(
                &db,
                pkg_id,
                1,
                BuildState::Enqueued,
                Some(100 + number as i64),
                None,
            )
            .await;
        }
        let done_id = insert_package(&db, "done-pkg", false, true).await;
        insert_build(
            &db,
            done_id,
            1,
            BuildState::Successful,
            Some(200),
            Some(260),
        )
        .await;

        let queue = queue_slice(&db).await.unwrap();
        assert_eq!(queue.depth, 7);
        assert_eq!(queue.oldest.len(), 5);
        // Oldest first: queued since 101 comes before 102.
        assert_eq!(queue.oldest[0].start_time, Some(101));
        assert_eq!(queue.oldest[4].start_time, Some(105));
        // No workers are registered, so no approved worker can take an
        // x86_64 build — the server names the missing arch.
        assert!(queue.oldest[0].waiting_reason.is_some());
    }

    #[tokio::test]
    async fn out_of_date_splits_on_global_setting_and_queued_build() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let pkg_a = insert_package(&db, "needs-hand", true, true).await;
        let pkg_c = insert_package(&db, "queued-anyway", true, true).await;
        insert_build(&db, pkg_c, 1, BuildState::Enqueued, Some(now_secs()), None).await;

        // By default both rebuild paths are on, so nothing needs a hand.
        let slice = out_of_date_slice(&db).await.unwrap();
        assert_eq!(slice.needs_hand.len(), 0);
        assert_eq!(slice.handled, 2);

        // Auto-rebuild off globally: A needs a hand, C is handled by its
        // queued build regardless of the setting.
        settings::write(
            &db,
            [
                settings::Change {
                    setting: Setting::BuildOnNewVersion,
                    pkg_id: None,
                    value: Some("false".to_string()),
                },
                settings::Change {
                    setting: Setting::AutoUpdateSchedule,
                    pkg_id: None,
                    value: Some(String::new()),
                },
            ],
        )
        .await
        .unwrap();
        let slice = out_of_date_slice(&db).await.unwrap();
        assert_eq!(slice.needs_hand.len(), 1);
        assert_eq!(slice.needs_hand[0].name, "needs-hand");
        assert_eq!(slice.handled, 1);

        // A per-package override does not change anything: neither
        // auto-rebuild job (`update_version_check.rs`, `auto_update.rs`)
        // consults one, only the global value, so this dashboard card must
        // not either -- an override here would show A as handled while
        // nothing actually rebuilds it.
        settings::write(
            &db,
            [settings::Change {
                setting: Setting::BuildOnNewVersion,
                pkg_id: Some(pkg_a),
                value: Some("true".to_string()),
            }],
        )
        .await
        .unwrap();
        let slice = out_of_date_slice(&db).await.unwrap();
        assert_eq!(slice.needs_hand.len(), 1);
        assert_eq!(slice.handled, 1);

        // Global on: A is handled too.
        settings::write(
            &db,
            [settings::Change {
                setting: Setting::BuildOnNewVersion,
                pkg_id: None,
                value: Some("true".to_string()),
            }],
        )
        .await
        .unwrap();
        let slice = out_of_date_slice(&db).await.unwrap();
        assert_eq!(slice.needs_hand.len(), 0);
        assert_eq!(slice.handled, 2);
    }

    /// A package whose rebuild attempt just failed still carries the
    /// out-of-date flag (`worker_complete.rs`/`publish.rs` only clear it on
    /// success), but it belongs on the Failed card: the frontend's own
    /// out-of-date badge only ever describes a successful build that upstream
    /// has moved past, and this card's "View all" link relies on the same
    /// rule to show the packages it counted.
    #[tokio::test]
    async fn out_of_date_excludes_a_failed_build() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        packages::ActiveModel {
            name: Set("stale-and-broken".to_string()),
            status: Set(BuildState::Failed),
            out_of_date: Set(true),
            upstream_version: Set(Some("1.0-1".to_string())),
            build_flags: Set(Default::default()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: "stale-and-broken".to_string(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert package");

        let slice = out_of_date_slice(&db).await.unwrap();
        assert!(slice.needs_hand.is_empty());
        assert_eq!(slice.handled, 0);
    }

    /// A package with no files sums to a NULL size (`total_artifact_size_expr`,
    /// `SUM` over zero rows), and SQLite and Postgres disagree on where NULL
    /// sorts in `ORDER BY … DESC` -- last on one, first on the other. The card
    /// excludes those rows rather than relying on the sort to place them
    /// correctly on both.
    #[tokio::test]
    async fn largest_excludes_a_package_with_no_recorded_size() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let sized = insert_package(&db, "has-files", false, true).await;
        insert_package(&db, "no-files", false, true).await;
        files::ActiveModel {
            filename: Set("has-files-1.0-1-x86_64.pkg.tar.zst".to_string()),
            platform: Set(Platform::X86_64),
            package_id: Set(sized),
            size: Set(Some(1024)),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert file");

        let largest = largest_packages(&db).await.unwrap();
        assert_eq!(
            largest
                .iter()
                .map(|pkg| pkg.name.as_str())
                .collect::<Vec<_>>(),
            vec!["has-files"]
        );
    }

    #[tokio::test]
    async fn longest_respects_window_and_pairs_previous_on_same_platform() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let now = now_secs();
        let pkg_id = insert_package(&db, "long-pkg", false, true).await;
        // Outside the 30-day window: long enough to win if the window were
        // not applied, so its absence proves the filter.
        insert_build(
            &db,
            pkg_id,
            1,
            BuildState::Successful,
            Some(now - 40 * 24 * 60 * 60),
            Some(now - 40 * 24 * 60 * 60 + 1000),
        )
        .await;
        insert_build(
            &db,
            pkg_id,
            2,
            BuildState::Successful,
            Some(now - 10 * 24 * 60 * 60),
            Some(now - 10 * 24 * 60 * 60 + 60),
        )
        .await;
        insert_build(
            &db,
            pkg_id,
            3,
            BuildState::Successful,
            Some(now - 24 * 60 * 60),
            Some(now - 24 * 60 * 60 + 120),
        )
        .await;
        let single_id = insert_package(&db, "single-pkg", false, true).await;
        insert_build(
            &db,
            single_id,
            1,
            BuildState::Successful,
            Some(now - 24 * 60 * 60),
            Some(now - 24 * 60 * 60 + 30),
        )
        .await;

        // Invalid intervals never compete with real durations in this ranking.
        insert_build(
            &db,
            pkg_id,
            4,
            BuildState::Successful,
            Some(now),
            Some(i64::MIN),
        )
        .await;
        let longest = longest_builds(&db).await.unwrap();
        assert_eq!(longest.len(), 3);
        // Ordered by duration DESC: 120, 60, 30. The 1000-second build is
        // outside the window and must not appear.
        assert_eq!(longest[0].build.pkg_name, "long-pkg");
        assert_eq!(longest[0].build.number, 3);
        assert_eq!(longest[0].previous_secs, Some(60));
        assert_eq!(longest[2].build.pkg_name, "single-pkg");
        assert_eq!(longest[2].previous_secs, None);
    }

    #[tokio::test]
    async fn problems_exclude_info() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let now = now_secs();
        for (kind, severity) in [
            ("test.info", Severity::Info),
            ("test.warn", Severity::Warning),
        ] {
            logs::ActiveModel {
                kind: Set(kind.to_string()),
                severity: Set(severity),
                message: Set(format!("{kind} happened")),
                data: Set("{}".to_string()),
                scope: Set(None),
                timestamp: Set(now),
                user: Set(None),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
        }

        let problems = problem_entries(&db).await.unwrap();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].kind, "test.warn");
    }

    #[tokio::test]
    async fn dashboard_slices_succeed_on_an_empty_db_and_hold_none() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        // Every slice is independently fallible in the route (`None` on
        // error), so each must succeed on an empty instance rather than
        // failing the whole page.
        assert!(recent_packages(&db).await.is_ok());
        assert!(recent_builds(&db).await.is_ok());
        assert!(failed_packages(&db).await.is_ok());
        assert!(out_of_date_slice(&db).await.is_ok());
        assert!(queue_slice(&db).await.is_ok());
        assert!(problem_entries(&db).await.is_ok());
        assert!(largest_packages(&db).await.is_ok());
        assert!(longest_builds(&db).await.is_ok());

        // The shape itself holds a failed section while the rest render:
        // one `None` must survive a round trip.
        let view = super::DashboardView {
            recent_packages: Some(Vec::new()),
            recent_builds: Some(Vec::new()),
            failed: Some(Vec::new()),
            out_of_date: Some(super::OutOfDateSlice {
                needs_hand: Vec::new(),
                handled: 0,
            }),
            queue: Some(super::QueueSlice {
                depth: 0,
                oldest: Vec::new(),
            }),
            problems: None,
            largest: Some(Vec::new()),
            longest: Some(Vec::new()),
        };
        let json = serde_json::to_string(&view).unwrap();
        let back: super::DashboardView = serde_json::from_str(&json).unwrap();
        assert!(back.problems.is_none());
        assert!(back.recent_packages.is_some());
    }

    /// Twelve months back from the current one, newest first, across a year
    /// boundary, each starting at its first second UTC.
    #[test]
    fn the_graph_covers_the_last_twelve_months() {
        let now = jiff::civil::date(2026, 2, 15)
            .at(12, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap();
        let months = graph_months(&now);
        assert_eq!(months.len(), 12);
        assert_eq!(
            months[0],
            GraphMonth {
                year: 2026,
                month: 2,
                start: "2026-02-01T00:00:00Z"
                    .parse::<jiff::Timestamp>()
                    .unwrap()
                    .as_second(),
            }
        );
        assert_eq!((months[1].year, months[1].month), (2026, 1));
        assert_eq!((months[2].year, months[2].month), (2025, 12));
        assert_eq!((months[11].year, months[11].month), (2025, 3));
    }

    /// Months are the server's, not UTC's: two hours east of UTC, February
    /// starts at 22:00 UTC on January 31st, so a build then counts in
    /// February, as the operator would say it ran.
    #[test]
    fn a_month_starts_at_local_midnight() {
        let zone = jiff::tz::TimeZone::fixed(jiff::tz::offset(2));
        let now = jiff::civil::date(2026, 2, 15)
            .at(12, 0, 0, 0)
            .to_zoned(zone)
            .unwrap();
        let february = graph_months(&now)[0];
        assert_eq!((february.year, february.month), (2026, 2));
        assert_eq!(
            february.start,
            "2026-01-31T22:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_second()
        );
    }

    /// The builder queries count what they used to: builds per month with
    /// their successes, and the trend of the last 30 days against the 30
    /// before.
    #[tokio::test]
    async fn graph_and_trends_count_builds_by_when_they_started() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let pkg = insert_package(&db, "p", false, true).await;
        let now = now_secs();
        const DAY: i64 = 24 * 60 * 60;
        // Two in the last 30 days, one before that; one failed.
        insert_build(
            &db,
            pkg,
            1,
            BuildState::Successful,
            Some(now - DAY),
            Some(now - DAY + 100),
        )
        .await;
        insert_build(
            &db,
            pkg,
            2,
            BuildState::Failed,
            Some(now - 2 * DAY),
            Some(now - 2 * DAY + 300),
        )
        .await;
        insert_build(
            &db,
            pkg,
            3,
            BuildState::Successful,
            Some(now - 40 * DAY),
            Some(now - 40 * DAY + 100),
        )
        .await;

        let points = get_graph_datapoints(&db).await.unwrap();
        assert_eq!(points.iter().map(|p| p.count).sum::<i32>(), 3);
        assert_eq!(points.iter().map(|p| p.successful).sum::<i32>(), 2);
        // Newest month first.
        assert!(
            points
                .windows(2)
                .all(|w| (w[0].year, w[0].month) > (w[1].year, w[1].month))
        );

        let trends = build_trends(&db).await.unwrap();
        // Two builds against one: up 100%.
        assert!((trends.count - 1.0).abs() < 1e-6, "{}", trends.count);
        // Average 200s against 100s: up 100%.
        assert!((trends.duration - 1.0).abs() < 1e-6, "{}", trends.duration);
    }
}
