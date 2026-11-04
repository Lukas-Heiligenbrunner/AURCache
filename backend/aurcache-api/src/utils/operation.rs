//! Running a long operation detached from the request that started it.
//!
//! A bulk add or a restore runs for minutes, so the request hands back the
//! operation's id and the work carries on in a task. Its entries are recorded
//! as they arrive (`aurcache_db::helpers::operations`), so an observer can
//! attach late, or leave, and miss nothing.

use crate::utils::error::{ApiError, err};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::api::operations::OperationProgress;
use aurcache_db::helpers::operations;
use aurcache_utils::services::Services;
use rocket::http::Status;
use sea_orm::DatabaseConnection;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tracing::warn;

/// Depth of the channel an operation's work reports its entries on.
///
/// One entry per package flows producer-to-recorder; entries are small and
/// the producer does network-heavy work per entry while the recorder writes
/// one row, so 64 caps memory without ever throttling in practice. Bounded
/// rather than unbounded so a stalled database cannot grow the queue without
/// limit.
const PROGRESS_CHANNEL_CAPACITY: usize = 64;

/// How far an operation has got, as its row records it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub completed: i32,
    pub failed: i32,
}

/// What an operation makes of its entries as they arrive.
pub(crate) trait Recorder<E>: Send + 'static {
    /// Take in one entry, and say what the counts are with it.
    fn record(&mut self, entry: &E) -> Counts;

    /// The work has ended, one way or another.
    fn finish(self)
    where
        Self: Sized,
    {
    }
}

/// Start an operation of `kind` over `total` items: record it, then run `work`
/// detached, and answer its id for the caller to poll.
///
/// `work` reports its entries on the sender it is given, and each goes through
/// `recorder` into the operation's row.
pub(crate) async fn start<E, F>(
    services: &Services,
    kind: &'static str,
    total: i32,
    work: impl FnOnce(mpsc::Sender<E>) -> F + Send + 'static,
    recorder: impl Recorder<E>,
) -> Result<i32, ApiError>
where
    E: Serialize + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let id = operations::create(&services.db, kind, total).await?;
    spawn(
        services.db.clone(),
        services.activity.clone(),
        id,
        kind,
        work,
        recorder,
    );
    Ok(id)
}

/// Operation `id`'s state and its entries from `after` onwards; a 404 when
/// there is no such operation of `kind`.
pub(crate) async fn progress<E: DeserializeOwned>(
    db: &DatabaseConnection,
    id: i32,
    kind: &str,
    after: usize,
) -> Result<OperationProgress<E>, ApiError> {
    let job = operations::get(db, id)
        .await?
        .filter(|job| job.kind == kind)
        .ok_or_else(|| err(Status::NotFound, format!("no {kind} operation {id}")))?;
    Ok(OperationProgress {
        id: job.id,
        total: job.total,
        completed: job.completed,
        failed: job.failed,
        finished: job.finished_at.is_some(),
        entries: operations::entries_after(&job.log, after),
    })
}

/// Run `work` and record each entry it reports through `recorder` into
/// operation `id` of `kind`.
///
/// The row is closed when the work ends -- including when it panicked, which
/// is exactly when a job must not be left claiming to be running.
fn spawn<E, F>(
    db: DatabaseConnection,
    activity: ActivityLog,
    id: i32,
    kind: &'static str,
    work: impl FnOnce(mpsc::Sender<E>) -> F + Send + 'static,
    mut recorder: impl Recorder<E>,
) where
    E: Serialize + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let (sender, mut entries) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let worker = tokio::spawn(work(sender));

        // Each entry is written as it arrives rather than batched to the end:
        // a job that reports nothing until it finishes reports nothing for the
        // whole time anyone would want to watch it.
        let mut counts = Counts::default();
        while let Some(entry) = entries.recv().await {
            counts = recorder.record(&entry);
            if let Err(e) =
                operations::append(&db, id, counts.completed, counts.failed, &[entry], false).await
            {
                warn!("could not record {kind} {id} progress: {e}");
            }
        }
        recorder.finish();

        if let Err(e) = worker.await {
            activity.emit(Event::OperationAborted {
                operation: kind.to_string(),
                error: e.to_string(),
            });
        }
        if let Err(e) =
            operations::append::<_, E>(&db, id, counts.completed, counts.failed, &[], true).await
        {
            warn!("could not close {kind} {id}: {e}");
        }
    });
}
