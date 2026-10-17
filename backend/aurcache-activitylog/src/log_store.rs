//! Reading the structured log: filtering and paging.
//!
//! Nothing here asks whether the entities an entry names still exist. Every
//! reference links, and the page it links to decides what to do when its
//! subject is gone -- a missing package offers to add it, a missing build falls
//! back to its package's builds. Existence answered here would be stale by the
//! time anyone clicked.
//!
//! Every query here is ordinary SQL that sea-orm builds the same way for both
//! backends. That is deliberate: nothing in this repository is tested against
//! Postgres, so a query that branched by backend would be one whose *tested*
//! branch is not the branch that runs in production. It is also why the entity
//! filter reads a side table instead of reaching into the payload JSON.
//!
//! See `design/implemented/structured-logs.md`.

use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::{LogEntry, LogPage, NS_BUILD, NS_PACKAGE, NS_WORKER};
use aurcache_db::prelude::{LogEntities, Logs};
use aurcache_db::{log_entities, logs};
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, Order, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, QueryTrait,
};

pub use aurcache_common::api::log::LogFilter;

/// Reading the log.
#[derive(Debug, Clone)]
pub struct LogStore {
    db: DatabaseConnection,
}

impl LogStore {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// One page, newest first, with how long the filtered log is.
    ///
    /// # Errors
    ///
    /// Returns any database error from the count, the page, or resolving links.
    pub async fn page(
        &self,
        limit: u64,
        offset: u64,
        filter: &LogFilter,
    ) -> anyhow::Result<LogPage> {
        let condition = self.condition(filter).await?;

        // After filtering, or the pager promises pages that are not there.
        let total = Logs::find()
            .filter(condition.clone())
            .count(&self.db)
            .await?;

        let rows = Logs::find()
            .filter(condition)
            .order_by(logs::Column::Timestamp, Order::Desc)
            // Ties broken by id: the log records whole seconds, and a burst
            // writes several within one, so without this a page boundary
            // between them could drop or repeat an entry.
            .order_by(logs::Column::Id, Order::Desc)
            .limit(limit)
            .offset(offset)
            .all(&self.db)
            .await?;

        let entries = rows.into_iter().map(render).collect();

        Ok(LogPage { entries, total })
    }

    /// The `where` clause for a filter.
    async fn condition(&self, filter: &LogFilter) -> anyhow::Result<Condition> {
        let mut condition = Condition::all();

        if let Some(floor) = filter.severity
            && floor != Severity::Info
        {
            condition = condition.add(logs::Column::Severity.gte(floor));
        }

        if filter.since_boot
            && let Some(booted) = self.last_start().await?
        {
            condition = condition.add(logs::Column::Timestamp.gte(booted));
        }

        if let Some(kind) = &filter.kind {
            condition = condition.add(logs::Column::Kind.eq(kind.as_str()));
        }

        if let Some(entity) = &filter.entity {
            // A subquery rather than a join, so the page is still one row per
            // entry when an entity appears under several roles.
            let mut refs = LogEntities::find()
                .select_only()
                .column(log_entities::Column::LogId)
                .filter(log_entities::Column::Ns.eq(entity.namespace()))
                .filter(log_entities::Column::Id.eq(entity.id()));
            if let Some(role) = &filter.role {
                refs = refs.filter(log_entities::Column::Role.eq(role.as_str()));
            }
            condition = condition.add(logs::Column::Id.in_subquery(refs.into_query()));
        }

        Ok(condition)
    }

    /// When this instance last started, as the log records it.
    ///
    /// `None` before anything has recorded a start, in which case "this boot"
    /// cannot mean anything and the filter does not narrow: showing everything
    /// beats showing nothing.
    async fn last_start(&self) -> anyhow::Result<Option<i64>> {
        // Only the timestamp: the row also carries the entry's data JSON.
        Ok(Logs::find()
            .select_only()
            .column(logs::Column::Timestamp)
            .filter(logs::Column::Kind.eq(crate::events::SERVER_START))
            .order_by(logs::Column::Timestamp, Order::Desc)
            .order_by(logs::Column::Id, Order::Desc)
            .into_tuple::<(i64,)>()
            .one(&self.db)
            .await?
            .map(|(timestamp,)| timestamp))
    }

    /// Delete entries older than `keep_secs`, returning how many went.
    ///
    /// The index rows go with them: `log_entity.log_id` cascades.
    ///
    /// # Errors
    ///
    /// Returns any database error from the delete.
    pub async fn prune(&self, keep_secs: u64, now: i64) -> anyhow::Result<u64> {
        if keep_secs == 0 {
            return Ok(0);
        }
        let cutoff = prune_cutoff(now, keep_secs);
        let deleted = Logs::delete_many()
            .filter(logs::Column::Timestamp.lt(cutoff))
            .exec(&self.db)
            .await?;
        Ok(deleted.rows_affected)
    }
}

/// Timestamps older than this are pruned. Saturating: the clock is trusted
/// here, and a far-future `now` must prune everything rather than wrap to
/// keeping it all.
fn prune_cutoff(now: i64, keep_secs: u64) -> i64 {
    now.saturating_sub(i64::try_from(keep_secs).unwrap_or(i64::MAX))
}

/// A stored row as the API returns it.
///
/// A payload that no longer parses becomes `null` rather than costing the entry:
/// `message` was rendered when it was written and still reads, which is the
/// whole reason it is stored.
fn render(row: logs::Model) -> LogEntry {
    let data = serde_json::from_str(&row.data).unwrap_or_else(|e| {
        tracing::warn!("log entry {} has an unreadable payload: {e}", row.id);
        serde_json::Value::Null
    });
    LogEntry {
        id: row.id,
        kind: row.kind,
        severity: row.severity,
        message: row.message,
        data,
        scope: row.scope.and_then(|raw| raw.parse().ok()),
        timestamp: row.timestamp,
        user: row.user,
    }
}

/// Namespaces, re-exported so a caller building a filter does not have to reach
/// into the common crate for them.
pub const NAMESPACES: [&str; 3] = [NS_PACKAGE, NS_WORKER, NS_BUILD];
