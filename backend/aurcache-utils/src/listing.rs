//! The projections every list of packages or builds shares, so one place
//! describes the shape of a listed row.

use crate::worker_policy::worker_policy;
use aurcache_common::api::builds::{BuildSummary, DiskUsage, KeptBuild};
use aurcache_common::api::waiting::WaitingReason;
use aurcache_common::build_state::BuildState;
use aurcache_db::helpers::builds::latest_successful_version_expr;
use aurcache_db::helpers::files::total_artifact_size_expr;
use aurcache_db::helpers::worker_jobs;
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, packages, workers};
use sea_orm::{
    DatabaseConnection, EntityTrait, FromQueryResult, JoinType, QuerySelect, RelationTrait, Select,
};
use std::collections::HashMap;

/// The package-list projection shared by the list and dashboard routes.
///
/// Every listed package has the same fields, so there is one place the shape
/// of a [`SimplePackage`] is described.
pub fn package_row_select() -> Select<Packages> {
    Packages::find()
        .select_only()
        .column(packages::Column::Name)
        .column(packages::Column::Status)
        .column(packages::Column::OutOfDate)
        .column(packages::Column::UpstreamVersion)
        .column(packages::Column::DirectlyRequested)
        // No COALESCE to an empty string: a package with no build has no
        // version, and `null` says that where `""` is indistinguishable from a
        // build that produced a blank one.
        .column_as(latest_successful_version_expr(), "built_version")
        .column_as(total_artifact_size_expr(), "total_size")
}

/// The build-list projection shared by the list, single-build and dashboard routes.
///
/// Every listed build has the same fields, so there is one place the shape of a
/// [`BuildRow`] is described.
pub fn build_row_select() -> Select<Builds> {
    Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .select_only()
        .column_as(builds::Column::Id, "id")
        .column_as(builds::Column::Number, "number")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column(builds::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .column(builds::Column::Platform)
        .column(builds::Column::Size)
        .column(builds::Column::PeakMemory)
        .column(builds::Column::DiskChroot)
        .column(builds::Column::DiskWorkdir)
        .column(builds::Column::DiskSources)
        .column(builds::Column::DiskBuildTree)
        .column(builds::Column::KeptPath)
        .column(builds::Column::KeptUntil)
        .column(builds::Column::KeptTree)
        // Left, so a queued build -- which has no worker yet -- still lists.
        .join(JoinType::LeftJoin, builds::Relation::Workers.def())
        .column_as(workers::Column::Name, "worker_name")
}

/// A listed build as queried, including the row id the response omits.
///
/// Waiting reasons are keyed by row id internally, so the id has to survive as
/// far as the annotation — but no further.
#[derive(FromQueryResult)]
pub struct BuildRow {
    pub id: i32,
    number: i32,
    pkg_name: String,
    version: String,
    pub status: BuildState,
    start_time: Option<i64>,
    end_time: Option<i64>,
    platform: String,
    size: Option<i64>,
    peak_memory: Option<i64>,
    disk_chroot: Option<i64>,
    disk_workdir: Option<i64>,
    disk_sources: Option<i64>,
    disk_build_tree: Option<i64>,
    kept_path: Option<String>,
    kept_until: Option<i64>,
    kept_tree: Option<String>,
    worker_name: Option<String>,
}

impl BuildRow {
    #[must_use]
    pub fn into_summary(self, waiting_reason: Option<WaitingReason>) -> BuildSummary {
        BuildSummary {
            number: self.number,
            pkg_name: self.pkg_name,
            version: self.version,
            status: self.status,
            start_time: self.start_time,
            end_time: self.end_time,
            platform: self.platform,
            size: self.size,
            peak_memory: self.peak_memory,
            disk_usage: Some(DiskUsage {
                chroot: self.disk_chroot,
                workdir: self.disk_workdir,
                sources: self.disk_sources,
                build_tree: self.disk_build_tree,
            })
            .filter(|usage| !usage.is_empty()),
            kept: self
                .kept_path
                .zip(self.kept_until)
                .map(|(path, until)| KeptBuild {
                    path,
                    until,
                    tree: self.kept_tree,
                }),
            worker_name: self.worker_name,
            // Filled only by the detail route, which knows it is rendering one
            // build; the lists would pay a stat per row for fields they do not
            // show. See the field's docs for how `None` differs from `0`.
            log_size: None,
            waiting_reason,
        }
    }
}

/// Attach [`WaitingReason`]s to any listed build that no approved worker can
/// currently claim, so a stalled build is distinguishable from a queued one.
///
/// Best-effort by design: this is diagnostic decoration, and failing to compute
/// it must never turn a working build list into an error page.
pub async fn annotate_waiting(db: &DatabaseConnection, rows: Vec<BuildRow>) -> Vec<BuildSummary> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut reasons = waiting_reasons(db).await;
    rows.into_iter()
        .map(|row| {
            let reason = reasons.remove(&row.id);
            row.into_summary(reason)
        })
        .collect()
}

/// Why each queued build is waiting, by row id; empty when that cannot be
/// computed.
pub async fn waiting_reasons(db: &DatabaseConnection) -> HashMap<i32, WaitingReason> {
    worker_jobs::waiting_reasons(db, worker_policy().liveness_timeout_secs)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("could not compute build waiting reasons: {e}");
            HashMap::new()
        })
}
