//! Reading and writing the progress of a long-running operation.
//!
//! The shape both bulk add and restore need: the request that starts the work
//! returns before any of it is done, the work runs detached, and progress is
//! *recorded* rather than pushed. Recording is what makes the rest true --
//! an observer can attach late and see the run from the beginning, one that
//! disconnects misses nothing, and an error that happened while nobody was
//! watching is still there afterwards. A pushed stream would need to buffer the
//! same log behind it to manage any of that.
//!
//! Generic over the entry type. The two kinds of operation report genuinely
//! different things -- a package was added or already existed, versus a package
//! was imported or skipped or overwritten -- and flattening both into one
//! stringly outcome would lose that. Only the transport is shared: one JSON
//! object per line, read by line offset.

use crate::helpers::time::now_secs;
use crate::operations;
use crate::prelude::Operations;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{BinOper, Expr, ExprTrait};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The kinds the `kind` column holds, named once for the server and its
/// clients alike.
pub use aurcache_common::api::operations::kind::{
    BULK_ADD as KIND_BULK_ADD, DEPENDENCY_BACKFILL as KIND_DEPENDENCY_BACKFILL,
    RESTORE as KIND_RESTORE,
};

/// Start an operation, returning its id.
///
/// `total` is fixed here because it is known before any work happens, and a
/// progress report without a denominator cannot say how far along it is.
pub async fn create<C: ConnectionTrait>(db: &C, kind: &str, total: i32) -> Result<i32, DbErr> {
    Ok(operations::ActiveModel {
        kind: Set(kind.to_string()),
        created_at: Set(now_secs()),
        total: Set(total),
        ..Default::default()
    }
    .insert(db)
    .await?
    .id)
}

/// Append outcomes and update the counters.
///
/// Called as each item finishes rather than once at the end. An operation that
/// runs for minutes and reports nothing until it stops reports nothing for the
/// whole time anyone would want to watch it, and the write is trivial beside
/// the work each entry represents.
///
/// One `UPDATE`, no read: concatenating in SQL keeps the write constant-size
/// no matter how long the log has grown (a read-modify-write would rewrite the
/// whole log per item), and `||` concatenates on both SQLite and Postgres.
/// Operation rows are never deleted, so a missing `id` writes nothing — the
/// same outcome as before, without a query to establish it.
pub async fn append<C: ConnectionTrait, T: Serialize>(
    db: &C,
    id: i32,
    completed: i32,
    failed: i32,
    entries: &[T],
    finished: bool,
) -> Result<(), DbErr> {
    let mut appended = String::new();
    for entry in entries {
        // An entry that will not serialise is dropped rather than failing
        // the operation: the work is done either way, and losing one line
        // of the report is better than abandoning the rest of the run.
        if let Ok(line) = serde_json::to_string(entry) {
            appended.push_str(&line);
            appended.push('\n');
        }
    }

    // No `Concat` variant in this sea-query: `Custom` renders verbatim, and
    // `||` concatenates text on both SQLite and Postgres.
    let log = Expr::col((operations::Entity, operations::Column::Log))
        .binary(BinOper::Custom("||"), appended);
    let mut update = operations::Entity::update_many()
        .col_expr(operations::Column::Log, log)
        .col_expr(operations::Column::Completed, completed.into())
        .col_expr(operations::Column::Failed, failed.into())
        .filter(operations::Column::Id.eq(id));
    if finished {
        update = update.col_expr(operations::Column::FinishedAt, Some(now_secs()).into());
    }
    update.exec(db).await?;
    Ok(())
}

/// The operation, if it exists.
pub async fn get<C: ConnectionTrait>(db: &C, id: i32) -> Result<Option<operations::Model>, DbErr> {
    Operations::find_by_id(id).one(db).await
}

/// The entries after the first `after` of them.
///
/// A caller holding none asks from zero and receives the whole run. An offset
/// past the end yields nothing rather than erroring, because a poll landing
/// between two writes is the normal case. A line that will not parse is
/// skipped: it was written by a version that knew a shape this one does not,
/// and the rest of the run is still worth reading.
#[must_use]
pub fn entries_after<T: DeserializeOwned>(log: &str, after: usize) -> Vec<T> {
    log.lines()
        .skip(after)
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Close out operations that were running when the server stopped.
///
/// The work ran in a task, so it died with the process. The row would otherwise
/// report itself running forever and an observer would poll for progress that
/// cannot come. Whatever was already done stays done -- each item is committed
/// as it goes.
///
/// A dependency backfill is the exception: the server starts it, not a
/// request, so it is started again rather than closed; see [`pending`].
pub async fn close_orphaned<C: ConnectionTrait>(db: &C) -> Result<u64, DbErr> {
    let res = Operations::update_many()
        .col_expr(operations::Column::FinishedAt, Some(now_secs()).into())
        .filter(operations::Column::FinishedAt.is_null())
        .filter(operations::Column::Kind.ne(KIND_DEPENDENCY_BACKFILL))
        .exec(db)
        .await?;
    Ok(res.rows_affected)
}

/// The oldest unfinished operation of `kind`, if one is waiting.
pub async fn pending<C: ConnectionTrait>(
    db: &C,
    kind: &str,
) -> Result<Option<operations::Model>, DbErr> {
    Operations::find()
        .filter(operations::Column::Kind.eq(kind))
        .filter(operations::Column::FinishedAt.is_null())
        .order_by_asc(operations::Column::Id)
        .one(db)
        .await
}

/// Operations still running, oldest first.
///
/// `finished_at IS NULL` is the whole definition of running, and startup closes
/// any row left open by a process that died (see [`close_orphaned`]), so this
/// cannot report a job that no longer exists.
///
/// Counters without the log: a caller listing what is in flight wants to draw
/// a progress bar, and a bulk add's log can be long enough that fetching every
/// one of them to list what is running would be the expensive part. Selected,
/// not just unmapped: the full rows would still cross the wire otherwise.
pub async fn active<C: ConnectionTrait>(
    db: &C,
) -> Result<Vec<aurcache_common::api::operations::ActiveOperation>, DbErr> {
    use aurcache_common::api::operations::ActiveOperation;
    Operations::find()
        .select_only()
        .column(operations::Column::Id)
        .column(operations::Column::Kind)
        .column(operations::Column::CreatedAt)
        .column(operations::Column::Total)
        .column(operations::Column::Completed)
        .column(operations::Column::Failed)
        .filter(operations::Column::FinishedAt.is_null())
        .order_by_asc(operations::Column::CreatedAt)
        .into_model::<ActiveOperation>()
        .all(db)
        .await
}

#[cfg(test)]
mod tests {
    use super::{KIND_BULK_ADD, append, create, entries_after, get};
    use crate::migration::Migrator;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
    struct Entry {
        name: String,
    }

    fn log(names: &[&str]) -> String {
        names
            .iter()
            .map(|n| {
                serde_json::to_string(&Entry {
                    name: (*n).to_string(),
                })
                .unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Attaching with nothing in hand gives the whole run, including what
    /// happened before anyone was watching.
    #[test]
    fn a_late_observer_sees_the_whole_run() {
        let seen: Vec<Entry> = entries_after(&log(&["a", "b", "c"]), 0);
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].name, "a");
    }

    /// Polling asks from what it holds, so it receives only what is new.
    #[test]
    fn an_offset_returns_only_what_is_new() {
        let seen: Vec<Entry> = entries_after(&log(&["a", "b"]), 1);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].name, "b");
    }

    /// A poll landing between writes is normal, not an error.
    #[test]
    fn an_offset_past_the_end_is_empty() {
        assert!(entries_after::<Entry>("", 0).is_empty());
        assert!(entries_after::<Entry>(&log(&["a"]), 5).is_empty());
    }

    /// A line this version cannot read does not cost the ones it can -- a dump
    /// restored across versions is the whole point of the feature.
    #[test]
    fn an_unreadable_line_is_skipped_not_fatal() {
        let mixed = format!("{}\n{{\"unknown\":true}}\n{}", log(&["a"]), log(&["b"]));
        let seen: Vec<Entry> = entries_after(&mixed, 0);
        assert_eq!(seen.len(), 2);
    }

    /// Appends accumulate in SQL rather than rewriting the log per item: two
    /// appends leave both lines, the counters both writes, and `finished_at`
    /// only the closing write. This also proves the `||` concatenation the
    /// single-statement append relies on.
    #[tokio::test]
    async fn appends_accumulate_in_sql() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let id = create(&db, KIND_BULK_ADD, 2).await.unwrap();

        append(&db, id, 1, 0, &[Entry { name: "a".into() }], false)
            .await
            .unwrap();
        append(&db, id, 1, 1, &[Entry { name: "b".into() }], true)
            .await
            .unwrap();

        let op = get(&db, id).await.unwrap().unwrap();
        assert_eq!(op.completed, 1);
        assert_eq!(op.failed, 1);
        assert!(op.finished_at.is_some());
        let seen: Vec<Entry> = entries_after(&op.log, 0);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].name, "a");
        assert_eq!(seen[1].name, "b");
    }

    /// Appending to a missing operation writes nothing and fails nothing --
    /// rows are never deleted, so this is only defence in depth, matching the
    /// old read-then-write's early return.
    #[tokio::test]
    async fn appending_to_a_missing_operation_is_a_no_op() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        append(&db, 999, 1, 0, &[Entry { name: "a".into() }], true)
            .await
            .unwrap();
    }
}
