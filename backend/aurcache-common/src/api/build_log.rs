//! Byte-window helpers for walking a build log in bounded pages.
//!
//! The `/output` endpoint serves raw file bytes sliced by byte offset. A page
//! can start or end inside a multi-byte UTF-8 character, and each consumer —
//! the CLI's pager and the frontend's log window — has to resolve that the
//! same way. The one function that needs to be identical everywhere, so the
//! offsets both ends compute always agree, lives here.

/// How much of a page's edges [`align`] holds back; see there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Alignment {
    pub front_skip: usize,
    pub back_drop: usize,
}

/// Resolve a raw page of build log to a UTF-8-aligned slice, and say how far
/// the page actually advanced.
///
/// Returns an [`Alignment`]:
///
/// - `front_skip` is how many leading bytes to drop before the slice is
///   displayable — the *tail* of a character that started on the previous
///   page. A page normally starts at a boundary (the start of the log, a
///   re-aligned offset, or `log_size - cap`), so this is usually zero.
/// - `back_drop` is how many trailing bytes to drop — the *start* of a
///   character that is still being written, or that got cut by the page's
///   `limit`. The caller advances its next offset by
///   `offset + raw.len() - back_drop`, and that re-reads the character whole
///   instead of printing two mangled halves.
///
/// The slice `&raw[front_skip..raw.len() - back_drop]` does not split a
/// character at either edge. Invalid bytes anywhere else are the log's
/// problem, left for the caller's lossy decode: only a character the page cut
/// short is held back, never everything after a bad byte. The two values sum
/// to at most `raw.len()`, and to all of it when the page is nothing but a
/// split character — which is why callers also stop when
/// `raw.len() - back_drop == 0`: no forward progress, so EOF mid-character
/// terminates rather than re-reading the same tail forever.
pub fn align(raw: &[u8]) -> Alignment {
    // Leading continuation bytes continue a character started before the
    // page. Full stop — that character cannot be recovered from what we have
    // been given, so it is dropped from the display.
    let front_skip = raw.iter().take_while(|b| is_continuation(**b)).count();
    Alignment {
        front_skip,
        back_drop: unfinished_suffix(&raw[front_skip..]),
    }
}

fn is_continuation(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

/// How many trailing bytes are the start of a character the page cut short.
///
/// A character is at most four bytes, so only the last three can hold a lead
/// byte still waiting for the rest. Whatever else is at the end -- a complete
/// character, or garbage -- is kept.
fn unfinished_suffix(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if is_continuation(byte) {
            continue;
        }
        let length = match byte {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if length > back { back } else { 0 };
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{Alignment, align};

    fn at(front_skip: usize, back_drop: usize) -> Alignment {
        Alignment {
            front_skip,
            back_drop,
        }
    }

    /// The trivial case: an already-aligned page is dropped as-is.
    #[test]
    fn a_complete_page_needs_no_adjustment() {
        assert_eq!(align(b""), at(0, 0));
        assert_eq!(align(b"hello\n"), at(0, 0));
        assert_eq!(
            align("unrecognized option \u{2018}a\u{2019}\n".as_bytes()),
            at(0, 0)
        );
    }

    /// The whole point of the rewind: cut a 3-byte character (U+2018, which
    /// gcc quotes its diagnostics with) and the page drops the split byte — but
    /// the rewind offset re-reads the character whole.
    #[test]
    fn a_split_character_is_dropped_and_re_read_whole() {
        let raw = "unrecognized option \u{2018}".as_bytes();
        // Cut after 'unrecognized option ' (index 20) plus the leading byte of
        // U+2018: the page is 21 bytes, of which the last is half a character.
        assert_eq!(align(&raw[..21]), at(0, 1));
        // `next_offset = 0 + 21 - 1 = 20` re-reads the character whole.
        assert_eq!(align(&raw[20..]), at(0, 0));
        assert_eq!(&raw[20..23], "\u{2018}".as_bytes());
    }

    /// A page whose first bytes continue a character from the previous page
    /// cannot recover that character; those bytes are dropped from display.
    #[test]
    fn leading_continuation_bytes_are_dropped() {
        let raw = b"\x80\x98-fno_char8_t\n";
        assert_eq!(align(raw), at(2, 0));
    }

    /// A page of nothing but one character's remaining bytes is all slack:
    /// nothing to display, and (for the display slice) no character at all.
    #[test]
    fn a_page_of_only_continuation_bytes_has_nothing_to_show() {
        let raw = b"\x80\x98";
        let aligned = align(raw);
        assert_eq!(aligned, at(2, 0));
        assert_eq!(&raw[aligned.front_skip..raw.len() - aligned.back_drop], b"");
    }

    /// The zero-progress case that makes EOF mid-character terminate: a page
    /// containing only the leading byte of a character neither displays nor
    /// advances the offset, so the walker must stop rather than re-read the
    /// same byte forever.
    #[test]
    fn an_entire_page_of_a_split_character_is_zero_progress() {
        let raw = b"\xE2";
        assert_eq!(align(raw), at(0, 1));
        assert_eq!(raw.len() - 1, 0);
    }

    /// A totally empty page advances nothing by definition; it is how a walker
    /// knows it has caught up with a log that ends on a character boundary.
    #[test]
    fn an_empty_page_is_zero_progress() {
        assert_eq!(align(b""), at(0, 0));
    }

    /// Garbage is left for the caller's lossy decode, wherever it is. Holding
    /// back everything after a bad byte would stall a walker for good: the
    /// next page would start at the same bad byte and advance nothing.
    #[test]
    fn garbage_is_kept_and_only_a_cut_character_is_held_back() {
        let raw = b"before\xFFafter";
        assert_eq!(align(&raw[..9]), at(0, 0));
        assert_eq!(align(b"\xFF"), at(0, 0));
        // Garbage earlier in the page does not hide a character cut at its end.
        let cut = [b"before\xFFafter ".as_slice(), &"\u{2018}".as_bytes()[..2]].concat();
        assert_eq!(align(&cut), at(0, 2));
        // A four-byte character is complete once all four bytes are there.
        let emoji = "\u{1F600}".as_bytes();
        assert_eq!(align(&emoji[..3]), at(0, 3));
        assert_eq!(align(emoji), at(0, 0));
    }
}
