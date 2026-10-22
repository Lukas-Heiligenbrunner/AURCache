//! Running a long operation detached from the request that started it.
//!
//! A bulk add or a restore runs for minutes, so the request hands back the
//! operation's id and the work carries on in a task. Its entries are recorded
//! as they arrive (`aurcache_db::helpers::operations`), so an observer can
//! attach late, or leave, and miss nothing.

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_db::helpers::operations;
use sea_orm::DatabaseConnection;
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::warn;

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

/// Run `work`, which reports its entries on the sender it is given, and record
/// each through `recorder` into operation `id` of `kind`.
///
/// The row is closed when the work ends -- including when it panicked, which
/// is exactly when a job must not be left claiming to be running.
pub(crate) fn spawn<E, F>(
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
        let (sender, mut entries) = mpsc::channel(crate::utils::PROGRESS_CHANNEL_CAPACITY);
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
