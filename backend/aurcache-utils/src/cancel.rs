//! Stopping a build at an operator's request.

use crate::build_logger::append_build_output;
use aurcache_common::build_state::{BuildState, EndReason};
use aurcache_db::builds;
use aurcache_db::helpers::builds::refresh_package_status;
use aurcache_db::helpers::time::now_secs;
use aurcache_db::prelude::Builds;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, TransactionTrait};
use tracing::warn;

/// What [`cancel_build`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cancel {
    /// The build is `FAILED`, ended by the operator.
    Cancelled,
    /// The build had already settled -- succeeded, failed, or moved on to
    /// publishing -- and was left as it was.
    Settled,
}

/// The states a build can still be cancelled from.
///
/// Not `PUBLISHING`: the worker is done with it and the server is putting it
/// in the repository, which is not something to interrupt halfway.
const CANCELLABLE: [BuildState; 3] = [
    BuildState::Active,
    BuildState::Enqueued,
    BuildState::WaitingForDeps,
];

/// Cancel `build`, which `pkgbase` names; one write is the decision point.
///
/// * The row goes `FAILED` with `end_reason = canceled` and keeps the
///   `worker_id` of whoever ran it -- the record of the aborted attempt, which
///   a later `complete` ack checks against.
/// * The write is guarded on the build still being cancellable, so a cancel
///   racing a completion can never retroactively fail a finished build: the
///   completion wins, this changes nothing and says [`Cancel::Settled`].
/// * The package's status follows in the same transaction. No fresh build is
///   queued: this is an operator's decision.
///
/// A queued build can no longer be claimed; a running one has left `ACTIVE`,
/// so its worker sees the cancel in its next heartbeat answer and aborts. The
/// lease reaper never touches it, since it only reclaims `ACTIVE` builds.
pub async fn cancel_build(
    db: &DatabaseConnection,
    pkgbase: &str,
    build: &builds::Model,
) -> anyhow::Result<Cancel> {
    let txn = db.begin().await?;
    let cas = Builds::update_many()
        .col_expr(builds::Column::Status, BuildState::Failed.into())
        .col_expr(builds::Column::EndTime, Some(now_secs()).into())
        .col_expr(builds::Column::EndReason, Some(EndReason::Canceled).into())
        .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into())
        .filter(builds::Column::Id.eq(build.id))
        .filter(builds::Column::Status.is_in(CANCELLABLE))
        .exec(&txn)
        .await?;
    if cas.rows_affected == 0 {
        txn.rollback().await?;
        return Ok(Cancel::Settled);
    }

    refresh_package_status(&txn, build.pkg_id).await?;
    txn.commit().await?;

    // The row is terminal now; appending the reason is best-effort and last.
    if let Err(e) = append_build_output(pkgbase, build.number, "Cancelled by operator.\n").await {
        warn!(
            "could not append the cancel note to {pkgbase}/{}: {e}",
            build.number
        );
    }
    Ok(Cancel::Cancelled)
}
