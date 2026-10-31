pub mod config;
pub mod error;
pub(crate) mod failed_requests;
pub(crate) mod operation;
pub mod pagination;

/// Depth of the progress channels behind the bulk-add and restore jobs.
///
/// One entry per package flows producer-to-recorder; entries are small and
/// the producer does network-heavy work per entry while the recorder writes
/// one row, so 64 caps memory without ever throttling in practice. Bounded
/// rather than unbounded so a stalled database cannot grow the queue without
/// limit.
pub const PROGRESS_CHANNEL_CAPACITY: usize = 64;
