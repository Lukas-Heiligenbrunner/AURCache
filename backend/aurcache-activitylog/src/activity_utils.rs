//! Recording to the structured log: the handle callers hold, and the one task
//! that writes.
//!
//! See `design/implemented/structured-logs.md`.

use crate::event::Reference;
use crate::events::Event;
use aurcache_db::prelude::LogEntities;
use aurcache_db::{log_entities, logs};
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, TransactionSession};

pub use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::EntityRef;

/// One entry on its way to the database, with the index rows it implies
/// already worked out.
///
/// Rendered and timestamped where it was emitted, not where it is written: a
/// backlog must not misdate what it is holding. The references are read from
/// the same serialized payload that gets stored, so the entry and its index
/// cannot describe different things.
#[derive(Debug, Clone)]
pub(crate) struct LogRecord {
    pub(crate) kind: &'static str,
    pub(crate) severity: Severity,
    pub(crate) message: String,
    pub(crate) data: String,
    pub(crate) scope: Option<EntityRef>,
    pub(crate) user: Option<String>,
    pub(crate) timestamp: i64,
    pub(crate) references: Vec<Reference>,
}

impl LogRecord {
    /// An event as it stands now, ready to be written.
    pub(crate) fn of(
        event: &Event,
        scope: Option<EntityRef>,
        user: Option<String>,
        timestamp: i64,
    ) -> Result<Self, serde_json::Error> {
        let rendered = crate::event::render(event)?;
        Ok(Self {
            kind: rendered.kind,
            severity: rendered.severity,
            message: rendered.message,
            data: rendered.payload.to_string(),
            scope,
            user,
            timestamp,
            references: rendered.references,
        })
    }
}

/// Where structured events appear in the journal.
///
/// One target for all of them rather than the emitting module, so a deployment
/// can turn the whole stream up or down in `LOG_LEVEL` without naming every
/// crate. The kind rides along as a field, and says more than a module path
/// would.
pub const EVENT_TARGET: &str = "aurcache::event";

/// The role an entry's own scope is indexed under.
///
/// A payload key, so it sits in the same namespace as the roles read out of the
/// payload; no event declares a field called this.
pub const SCOPE_ROLE: &str = "scope";

/// How many entries may be waiting to be written.
///
/// Generous: the log takes a few hundred entries on a busy day, so reaching
/// this means the database is not answering, which is a problem the log is not
/// going to fix by holding on.
const QUEUE: usize = 1024;

/// A handle for recording events, which knows nothing about the database.
///
/// Recording is what callers do everywhere -- in a publish, in a heartbeat, in
/// a scheduler pass -- and none of those places should be writing rows or
/// deciding what to do when a write fails. So [`Self::emit`] is synchronous
/// and infallible, and one task owns the connection and the error handling.
///
/// Cheap to clone; every clone feeds the same writer.
#[derive(Debug, Clone)]
pub struct ActivityLog {
    tx: tokio::sync::mpsc::Sender<LogRecord>,
    /// What everything recorded through this handle happened *under*.
    ///
    /// Set by [`Self::scoped`], so a build's own handle files everything it
    /// emits against that build without each call site repeating it.
    scope: Option<EntityRef>,
}

impl ActivityLog {
    /// Record an event the server noticed or did on its own.
    ///
    /// Does not block, does not fail, does not await. Every caller is
    /// describing something that *already happened*: an approval that went
    /// through and was not logged is a gap in the record, while one reported as
    /// failed after the fact is a lie about the instance. So there is nothing
    /// useful for a caller to do about a write that did not land, and nothing
    /// is asked of them.
    pub fn emit(&self, event: impl Into<Event>) {
        self.emit_by(event, None);
    }

    /// Record an event somebody set in motion.
    ///
    /// The actor matters for anything a person did -- a dependency repointed
    /// through the API is not the same entry as the scheduler doing it -- and
    /// is `None` for everything the server does on its own.
    pub fn emit_by(&self, event: impl Into<Event>, user: Option<String>) {
        let event = event.into();
        let record = match LogRecord::of(
            &event,
            self.scope.clone(),
            user,
            aurcache_db::helpers::time::now_secs(),
        ) {
            Ok(record) => record,
            Err(e) => {
                tracing::warn!("could not render a {} log event: {e}", event.kind());
                return;
            }
        };
        // The journal keeps its line, so `docker logs` and `journalctl` show
        // what they always did and a call site formats the sentence once. The
        // target is fixed rather than the emitting module, with the kind as a
        // field: `deps.replaced` says more about what happened than
        // `aurcache_api::package` does, and it is what a filter would rather
        // match on.
        match record.severity {
            Severity::Info => {
                tracing::info!(target: EVENT_TARGET, kind = record.kind, "{}", record.message);
            }
            Severity::Warning => {
                tracing::warn!(target: EVENT_TARGET, kind = record.kind, "{}", record.message);
            }
            Severity::Error => {
                tracing::error!(target: EVENT_TARGET, kind = record.kind, "{}", record.message);
            }
        }

        // Dropped rather than waited on: a log must never be the reason the
        // thing it is recording got slower. A full queue means the database is
        // not answering, and the journal still has the line above.
        if self.tx.try_send(record).is_err() {
            tracing::warn!("log queue is full or closed; an entry was dropped");
        }
    }

    /// A handle whose entries are all filed under one entity.
    ///
    /// `log.scoped(BuildRef { .. })` makes everything emitted through it part of
    /// that build's story, so one query returns both the entries *about* a build
    /// and those written *during* it. Scopes do not nest: the innermost handle
    /// wins, which is what a caller holding a build's handle means by it.
    #[must_use]
    pub fn scoped(&self, entity: impl Into<EntityRef>) -> Self {
        Self {
            tx: self.tx.clone(),
            scope: Some(entity.into()),
        }
    }

    /// A handle with nothing on the other end, whose entries go nowhere.
    ///
    /// For tests, and for anything run outside a server process. Named for what
    /// it does so it cannot be reached for by accident: a deployment wiring
    /// this in would have a log that silently stayed empty.
    #[must_use]
    pub fn discarding() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        Self { tx, scope: None }
    }
}

/// The one task that writes the log, and a handle to record through it.
pub struct Writer {
    pub log: ActivityLog,
    /// Ends when the last handle is dropped, draining what is queued first.
    pub task: tokio::task::JoinHandle<()>,
}

/// Start the one task that writes the log.
#[must_use]
pub fn spawn(db: DatabaseConnection) -> Writer {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LogRecord>(QUEUE);
    let task = tokio::spawn(async move {
        while let Some(record) = rx.recv().await {
            if let Err(e) = write(&db, record).await {
                tracing::warn!("could not write to the log: {e}");
            }
        }
    });
    Writer {
        log: ActivityLog { tx, scope: None },
        task,
    }
}

/// Write one entry and the index of what it refers to.
///
/// Both in one transaction: an entry that exists but is not indexed would be
/// invisible to every entity filter, which is worse than not having been
/// written at all, because nothing would show it was missing.
pub(crate) async fn write<C: sea_orm::ConnectionTrait + sea_orm::TransactionTrait>(
    db: &C,
    record: LogRecord,
) -> anyhow::Result<()> {
    // The scope is indexed like any other reference, under a role of its own,
    // so one query answers both "about this build" and "during it".
    let mut refs = record.references;
    if let Some(scope) = &record.scope {
        refs.extend(Reference::filed(SCOPE_ROLE, scope.clone()));
    }
    // A role may legitimately name one entity twice -- a list with a repeat --
    // and the index keys on the three together.
    refs.sort();
    refs.dedup();

    let txn = db.begin().await?;
    let entry = logs::ActiveModel {
        kind: Set(record.kind.to_string()),
        severity: Set(record.severity),
        message: Set(record.message),
        data: Set(record.data),
        scope: Set(record.scope.map(|scope| scope.to_string())),
        timestamp: Set(record.timestamp),
        user: Set(record.user),
        ..Default::default()
    }
    .insert(&txn)
    .await?;

    if !refs.is_empty() {
        let rows = refs
            .into_iter()
            .map(|Reference { role, entity }| log_entities::ActiveModel {
                log_id: Set(entry.id),
                role: Set(role),
                ns: Set(entity.namespace().to_string()),
                id: Set(entity.id()),
            });
        LogEntities::insert_many(rows).exec(&txn).await?;
    }

    txn.commit().await?;
    Ok(())
}
