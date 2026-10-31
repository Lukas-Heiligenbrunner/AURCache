//! Shared wall-clock helper.
//!
//! Lives here because `aurcache-db` is the lowest crate every server-side
//! consumer already depends on, and everything that calls this stores the
//! result in a timestamp column.
//!
//! `aurcache-worker-core` keeps its own four-line copy, and should: both crates
//! do depend on `aurcache-common`, so this *could* move there, but the worker's
//! version returns `u64` for an `AtomicU64` it subtracts with `saturating_sub`
//! to get an age, while this one returns `i64` for signed SQL columns. Sharing
//! one of them would buy four deduplicated lines at the price of a cast at
//! every call site on the other side -- and an `i64`-to-`u64` cast of a
//! backwards clock turns "0 seconds" into several billion.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
///
/// A clock set before 1970 yields `0` rather than an error: every caller stores
/// this in a timestamp column where a bogus-but-ordered value is more useful
/// than failing the surrounding operation.
#[must_use]
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
