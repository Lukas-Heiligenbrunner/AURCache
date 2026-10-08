//! Log-tail window management.
//!
//! The build-log screen fetches a bounded tail of the log — 4 MiB on phone,
//! 16 MiB on desktop — and walks it forward in aligned pages. The window is a
//! `String` that grows until it exceeds its cap, at which point whole leading
//! lines are drained to keep the DOM under a fixed memory budget.
//!
//! `align` lives in `aurcache_common::api::build_log` so the CLI uses the same
//! offset arithmetic the frontend does. `log_tail_cap` and `append_capped`
//! belong here because they touch the DOM: `append_capped` operates on a
//! `String` in place and the cap is determined at mount time from a browser
//! measurement. All three are pure functions and are tested without Dioxus.

/// A page whose size in bytes exceeds this value is considered a coarse pointer
/// device (touch screen), even if the screen is large. The frontend reads this
/// once at mount via `window.match_media("(pointer: coarse)")`.
const WIDTH_BREAKPOINT_PX: f64 = 768.0;

/// The tail budget on a phone (or narrow screen, or coarse pointer).
pub const MOBILE_LOG_TAIL_BYTES: usize = 4 << 20;

/// The tail budget on a desktop.
pub const DESKTOP_LOG_TAIL_BYTES: usize = 16 << 20;

/// The number of bytes to keep in the build-log window for this screen size.
///
/// The caller provides the viewport width (from `window.inner_width()`) and
/// whether the primary pointer is coarse (from `match_media("(pointer: coarse)")`).
/// `width` is `None` outside a window (SSR, tests) or when the measurement
/// fails, which reads as a phone.
pub fn log_tail_cap(width: Option<f64>, coarse_pointer: bool) -> usize {
    let narrow = width.is_none_or(|w| w < WIDTH_BREAKPOINT_PX);
    if narrow || coarse_pointer {
        MOBILE_LOG_TAIL_BYTES
    } else {
        DESKTOP_LOG_TAIL_BYTES
    }
}

/// Append `chunk` (already UTF-8 aligned) to `window`, then drain whole leading
/// lines until `window.len() <= cap`. Returns the number of bytes discarded;
/// discarding any bytes makes the window a "true tail", which the Copy button
/// and the footer readout key off.
///
/// - If a chunk is larger than `cap`, the window is trimmed immediately after
///   appending.
/// - The trim drains a whole line from the front (`\n` + 1), so line breaks are
///   never split.
/// - The one-line fallback (a single line longer than `cap`) byte-trims from the
///   front to a character boundary, so the remaining window is always UTF-8
///   valid even though the cut may land a byte or two shy of the cap.
pub fn append_capped(window: &mut String, chunk: &str, cap: usize) -> usize {
    window.push_str(chunk);
    if window.len() <= cap {
        return 0;
    }
    // One drain, not one per line: each `drain(..)` memmoves the whole tail,
    // so trimming a window of short lines line-by-line is quadratic in the
    // window size on every append. Cut once, right after the first newline
    // that removes at least the excess — the remainder still starts on a
    // line boundary, so what stays is whole trailing lines.
    // (`match_indices` only yields char boundaries, and '\n' is ASCII, so
    // the cut is always safe to drain.)
    let excess = window.len() - cap;
    let cut = if let Some((nl, _)) = window.match_indices('\n').find(|(i, _)| i + 1 >= excess) {
        nl + 1
    } else {
        // One unbreakable line longer than the cap: trim bytes from the
        // front. `drain` demands a char boundary, and the first boundary
        // at or past `excess` cuts the smallest whole-character window
        // that fits the cap — it stays within budget and never leaves a
        // sliced character at the window's start.
        window
            .char_indices()
            .map(|(i, _)| i)
            .find(|i| *i >= excess)
            .unwrap_or(window.len())
    };
    window.drain(..cut);
    cut
}

/// Whether the primary pointer is coarse (touch screen), the second half of the
/// cap measurement. Uses the same `match_media` trip the theme makes for
/// `(prefers-color-scheme: dark)`; false outside a window (SSR, tests).
pub fn coarse_pointer() -> bool {
    web_sys::window()
        .and_then(|w| w.match_media("(pointer: coarse)").ok().flatten())
        .is_some_and(|m| m.matches())
}

/// Where to fetch from instead of `next_offset`, when the log is more than a
/// window ahead of it.
///
/// Walking a backlog page by page would take a poll per window, and a build that
/// settles meanwhile gets only one more page -- so its last lines would never be
/// shown. Jumping to `log_size - cap` shows the end in one fetch. `None` means
/// carry on from `next_offset`, which includes a log whose size is not known.
pub fn catch_up_offset(next_offset: u64, log_size: Option<u64>, cap: u64) -> Option<u64> {
    let size = log_size?;
    (size.saturating_sub(next_offset) > cap).then(|| size - cap)
}

/// Drop everything through the first newline, so the window starts at a line
/// boundary.
///
/// Used once, when the initial frame is a tail: the window opens at
/// `offset = log_size − cap`, which lands mid-line, and the partial leading
/// line is not worth showing — the view should begin at a line start.
pub fn drop_leading_partial_line(window: &mut String) {
    if let Some(end) = window.find('\n') {
        window.drain(..=end);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DESKTOP_LOG_TAIL_BYTES, MOBILE_LOG_TAIL_BYTES, append_capped, catch_up_offset,
        drop_leading_partial_line, log_tail_cap,
    };

    #[test]
    fn a_log_more_than_a_window_ahead_is_jumped_to_its_end() {
        // First frame of a log bigger than the window: open at its end.
        assert_eq!(catch_up_offset(0, Some(100), 30), Some(70));
        // Within a window of where the view got to: walk on from there.
        assert_eq!(catch_up_offset(80, Some(100), 30), None);
        assert_eq!(catch_up_offset(0, Some(30), 30), None);
        // Follow was off while the build wrote far past the view.
        assert_eq!(catch_up_offset(10, Some(1000), 30), Some(970));
        // Not known, or a log that shrank (removed and rewritten): carry on.
        assert_eq!(catch_up_offset(0, None, 30), None);
        assert_eq!(catch_up_offset(500, Some(100), 30), None);
    }

    #[test]
    fn phone_sizes_are_smaller() {
        const { assert!(MOBILE_LOG_TAIL_BYTES < DESKTOP_LOG_TAIL_BYTES) };
        // Sanity: the caps are powers of two within MiB.
        assert_eq!(MOBILE_LOG_TAIL_BYTES, 4 << 20);
        assert_eq!(DESKTOP_LOG_TAIL_BYTES, 16 << 20);
    }

    #[test]
    fn log_tail_cap_picks_mobile_when_narrow_or_coarse() {
        assert_eq!(log_tail_cap(None, false), MOBILE_LOG_TAIL_BYTES);
        assert_eq!(log_tail_cap(Some(500.0), false), MOBILE_LOG_TAIL_BYTES);
        assert_eq!(log_tail_cap(Some(1000.0), true), MOBILE_LOG_TAIL_BYTES);
        assert_eq!(log_tail_cap(Some(1000.0), false), DESKTOP_LOG_TAIL_BYTES);
    }

    #[test]
    fn append_capped_trims_whole_lines() {
        let mut w = String::new();
        // Cap is 12, content is 29 bytes (3 lines) — well over the cap.
        let discarded_bytes = append_capped(&mut w, "line one\nline two\nline three\n", 12);
        assert_eq!(discarded_bytes, "line one\nline two\n".len());
        assert!(w.len() <= 12);
        // The drain happens on '\n' boundaries, so what remains is a full
        // trailing line, not a fragment.
        assert_eq!(w, "line three\n");
    }

    #[test]
    fn append_capped_reports_no_trim_when_within_cap() {
        let mut w = String::new();
        let discarded_bytes = append_capped(&mut w, "whole window\n", 100);
        assert_eq!(discarded_bytes, 0); // an untrimmed window is not a tail
        assert_eq!(w, "whole window\n");
    }

    #[test]
    fn append_capped_bytes_trims_single_long_line() {
        let mut w = String::new();
        let long = "A".repeat(50);
        let discarded_bytes = append_capped(&mut w, &long, 20);
        assert_eq!(discarded_bytes, 30);
        assert!(w.len() <= 20);
        assert!(w.chars().all(|c| c == 'A'));
    }

    #[test]
    fn append_capped_preserves_utf8_on_byte_trim() {
        let mut w = String::new();
        // U+2018 is 3 bytes. Make a line of them long enough to exceed the cap.
        let chunk: String = "\u{2018}".repeat(100);
        append_capped(&mut w, &chunk, 50);
        assert!(w.len() <= 50);
        // Must remain valid UTF-8 (the char-boundary drain handled it).
        assert!(std::str::from_utf8(w.as_bytes()).is_ok());
    }

    #[test]
    fn append_capped_keeps_whole_lines_across_appends() {
        let mut w = String::new();
        // Many short lines at once: one drain must leave whole trailing
        // lines within the cap, not fragments.
        let chunk: String = (0..100).map(|i| format!("line {i:03}\n")).collect();
        assert!(append_capped(&mut w, &chunk, 100) > 0);
        assert!(w.len() <= 100);
        assert!(w.ends_with('\n'));
        assert!(w.starts_with("line "));
        // And appending again keeps it bounded with whole lines.
        assert!(append_capped(&mut w, "tail line here\n", 100) > 0);
        assert!(w.len() <= 100);
        assert!(w.ends_with("tail line here\n"));
    }

    #[test]
    fn append_capped_zero_cap_trims_everything() {
        let mut w = "keep\n".to_string();
        let discarded_bytes = append_capped(&mut w, "new", 0);
        // With cap 0, every append results in the window being drained back to
        // empty or near-empty. The chunk "new" is 3 bytes > 0, so the byte-trim
        // drains it all; the result is empty.
        assert_eq!(discarded_bytes, "keep\nnew".len());
        assert!(w.is_empty());
    }

    #[test]
    fn drop_leading_partial_line_starts_at_a_line_boundary() {
        let mut w = "mid-line text\nline two\nline three\n".to_string();
        drop_leading_partial_line(&mut w);
        assert_eq!(w, "line two\nline three\n");

        // No newline: nothing is dropped, and without one the window cannot be
        // re-bounded, so the caller's byte-trim stays responsible for the cap.
        let mut single = "just one long line".to_string();
        drop_leading_partial_line(&mut single);
        assert_eq!(single, "just one long line");
    }
}

/// Byte position in a UTF-8 log, independent of the displayed window length.
#[derive(Debug, Default)]
pub struct LogCursor {
    pub next_offset: u64,
}

/// Whether a metadata refresh requires a new display window.
#[derive(Debug, PartialEq, Eq)]
pub enum WindowPosition {
    Continue,
    Restart { offset: u64 },
}

/// What appending a fetched page did to the display window.
pub struct WindowChange {
    pub is_tail: bool,
}

impl LogCursor {
    /// Reposition for a truncated log or a backlog larger than the window.
    pub fn position(&mut self, size: Option<u64>, cap: u64) -> WindowPosition {
        let offset = if size.is_some_and(|size| size < self.next_offset) {
            Some(0)
        } else {
            catch_up_offset(self.next_offset, size, cap)
        };
        match offset {
            Some(offset) => {
                self.next_offset = offset;
                WindowPosition::Restart { offset }
            }
            None => WindowPosition::Continue,
        }
    }

    /// Append a raw page and advance only past complete UTF-8 code points.
    pub fn append_page(&mut self, window: &mut String, page: &[u8], cap: usize) -> WindowChange {
        let alignment = aurcache_common::api::build_log::align(page);
        let end = page.len() - alignment.back_drop;
        let first_tail = self.next_offset > 0 && window.is_empty();
        let chunk = String::from_utf8_lossy(&page[alignment.front_skip..end]);
        let discarded = append_capped(window, &chunk, cap);
        if first_tail {
            drop_leading_partial_line(window);
        }
        self.next_offset = self.next_offset.saturating_add(end as u64);
        WindowChange {
            is_tail: discarded > 0 || first_tail,
        }
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;
    #[test]
    fn incomplete_utf8_is_retried_and_a_shrinking_log_restarts() {
        let mut cursor = LogCursor::default();
        let mut text = String::new();
        cursor.append_page(&mut text, &[b'a', 0xc3], 16);
        assert_eq!(cursor.next_offset, 1);
        cursor.append_page(&mut text, &[0xc3, 0xa9], 16);
        assert_eq!(cursor.next_offset, 3);
        assert_eq!(text, "aé");
        assert_eq!(
            cursor.position(Some(1), 16),
            WindowPosition::Restart { offset: 0 }
        );
    }
    #[test]
    fn resuming_after_a_backlog_opens_at_its_tail() {
        let mut cursor = LogCursor { next_offset: 10 };
        assert_eq!(
            cursor.position(Some(100), 20),
            WindowPosition::Restart { offset: 80 }
        );
    }
}
