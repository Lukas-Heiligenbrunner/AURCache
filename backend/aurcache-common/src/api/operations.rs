//! Long-running operations that are still in flight.
//!
//! Bulk adds and restores run detached: the request that starts one returns
//! immediately and the work carries on server-side, so a browser that navigated
//! away, reloaded, or was never the one that started it has no way to know
//! something is running. Listing them is what makes a job findable again.

use serde::{Deserialize, Serialize};

/// What kind of work an operation is.
///
/// A string rather than an enum on the wire, matching how the row stores it:
/// an older server can report a kind a newer client does not know, and the
/// client can say so rather than failing to parse the list.
pub mod kind {
    /// Adding several packages in one request.
    pub const BULK_ADD: &str = "bulk_add";
    /// Restoring an instance from a dump.
    pub const RESTORE: &str = "restore";
    /// Recording the dependencies of packages from before AURCache tracked
    /// them; queued by the upgrade from 0.5.0 and run once at startup.
    pub const DEPENDENCY_BACKFILL: &str = "dependency_backfill";
}

/// One operation in flight.
///
/// Counters rather than the log: a caller listing what is running wants to draw
/// a progress bar, and a bulk add's log can be long enough that sending all of
/// it to every such caller would be the expensive part. The id is what to poll
/// for the entries themselves.
///
/// Read straight off the row by the operations helper: the selected columns
/// are exactly these fields.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct ActiveOperation {
    pub id: i32,
    /// See [`kind`]. Unrecognised values are reported as they are.
    pub kind: String,
    /// Unix seconds.
    pub created_at: i64,
    pub total: i32,
    pub completed: i32,
    pub failed: i32,
}

impl ActiveOperation {
    /// Items resolved so far, however they turned out.
    #[must_use]
    pub const fn resolved_count(&self) -> i32 {
        self.completed + self.failed
    }
}

/// An operation's state, and its entries after the offset the caller asked
/// from.
///
/// One shape for every kind of operation: only the entries differ, as `E`.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OperationProgress<E> {
    pub id: i32,
    pub total: i32,
    pub completed: i32,
    pub failed: i32,
    /// Whether the job has stopped -- successfully or not. A job whose server
    /// restarted mid-run is finished too, with the unreached items recorded as
    /// failures rather than left pending forever.
    pub finished: bool,
    /// Entries from the requested offset onwards, so a caller polling only ever
    /// receives what it has not already seen.
    pub entries: Vec<E>,
}
