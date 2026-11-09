//! Page sizes and offsets for list endpoints.
//!
//! [`clamp_limit`] is a ceiling *and* a default, for lists that are read a page
//! at a time. Not for the builds and packages lists: the frontend fetches those whole
//! and filters, sorts and paginates them itself (`frontend-rs/src/listing.rs`),
//! so a default page there would silently cut them off.

/// Largest page any list endpoint returns.
pub const MAX_LIMIT: u64 = 500;

/// Page size when the caller does not say.
pub const DEFAULT_LIMIT: u64 = 100;

/// Resolve a caller-supplied `limit` to a bounded page size.
///
/// A missing `limit` reads as the default rather than "everything", so `page`
/// without `limit` pages through defaults instead of being silently ignored.
#[must_use]
pub fn clamp_limit(limit: Option<u64>) -> u64 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// The row offset of `page` for pages of `limit` rows; none without a limit.
///
/// Saturating: user input must never reach unchecked arithmetic -- a huge
/// `page` would wrap the offset in release or panic in debug.
#[must_use]
pub fn page_offset(page: Option<u64>, limit: Option<u64>) -> Option<u64> {
    page.zip(limit)
        .map(|(page, limit)| page.saturating_mul(limit))
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LIMIT, MAX_LIMIT, clamp_limit};

    /// No request may ask for an unbounded page: missing reads as the
    /// default, zero reads as one row, and anything huge clamps to the max.
    #[test]
    fn limits_are_always_bounded() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(10)), 10);
        assert_eq!(clamp_limit(Some(MAX_LIMIT)), MAX_LIMIT);
        assert_eq!(clamp_limit(Some(u64::MAX)), MAX_LIMIT);
    }
}
