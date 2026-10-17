//! Formatting shared by the screens that show build timing.

/// Now, in Unix **seconds** rather than milliseconds: the unit every
/// timestamp the server sends is in.
pub fn now_secs() -> i64 {
    (js_sys::Date::now() / 1000.0) as i64
}

/// Render a known runtime in the largest useful unit; a dash means unknown.
pub fn format_duration(duration: Option<std::time::Duration>) -> String {
    let Some(duration) = duration else {
        return "—".to_string();
    };
    let secs = duration.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// A byte count, at the largest unit that leaves a number worth reading.
///
/// Binary units, because that is what a package repository on disk is measured
/// in and what `du` will tell you. One decimal below 10 so `1.4 GiB` does not
/// round to the same thing as `1.9 GiB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// A size the server may not know, as the API sends one: a dash for `None`,
/// which is "nothing recorded" and never "zero bytes".
pub fn format_size(bytes: Option<i64>) -> String {
    bytes
        .and_then(|bytes| u64::try_from(bytes).ok())
        .map_or_else(|| "—".to_string(), format_bytes)
}

/// How long ago something happened, relative to `now`.
///
/// Relative rather than absolute because it avoids a timezone and locale
/// story, and "3h ago" is what you actually want when scanning a build list.
pub fn format_age(ts: Option<i64>, now: i64) -> String {
    let Some(ts) = ts else {
        return "—".to_string();
    };
    let secs = now.saturating_sub(ts);
    if secs < 0 {
        // Clock skew again: a build stamped in the future.
        return "just now".to_string();
    }
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::format_bytes;

    #[test]
    fn bytes_climb_to_the_largest_readable_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        // Past ten, the decimal stops earning its place.
        assert_eq!(format_bytes(20 * 1024), "20 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        // Nothing larger than TiB, so a huge value stays in it rather than
        // running off the end of the table.
        assert_eq!(format_bytes(5 * 1024_u64.pow(5)), "5120 TiB");
    }

    /// Zero is "no builds have finished", not "they took no time".
    #[test]
    fn zero_is_measured_and_unknown_is_explicit() {
        assert_eq!(format_duration(None), "—");
        assert_eq!(format_duration(Some(std::time::Duration::ZERO)), "0s");
        assert_eq!(
            format_duration(Some(std::time::Duration::from_secs(45))),
            "45s"
        );
        assert_eq!(
            format_duration(Some(std::time::Duration::from_secs(90))),
            "1m 30s"
        );
        assert_eq!(
            format_duration(Some(std::time::Duration::from_secs(3700))),
            "1h 1m"
        );
    }

    use super::*;

    #[test]
    fn durations_read_in_the_largest_useful_unit() {
        assert_eq!(
            format_duration(aurcache_common::api::builds::elapsed_between(
                Some(0),
                Some(43)
            )),
            "43s"
        );
        assert_eq!(
            format_duration(aurcache_common::api::builds::elapsed_between(
                Some(0),
                Some(123)
            )),
            "2m 3s"
        );
        assert_eq!(
            format_duration(aurcache_common::api::builds::elapsed_between(
                Some(100),
                Some(100)
            )),
            "0s"
        );
        assert_eq!(
            format_duration(aurcache_common::api::builds::elapsed_between(
                Some(0),
                Some(3600)
            )),
            "1h 0m"
        );
        assert_eq!(
            format_duration(aurcache_common::api::builds::elapsed_between(
                Some(0),
                Some(7860)
            )),
            "2h 11m"
        );
    }

    /// A running build has a start but no end. That is unknown, not zero — a
    /// build in progress must not read as having taken no time.
    #[test]
    fn an_unfinished_build_has_no_duration() {
        assert_eq!(
            format_duration(aurcache_common::api::builds::elapsed_between(
                Some(100),
                None
            )),
            "—"
        );
        assert_eq!(
            format_duration(aurcache_common::api::builds::elapsed_between(None, None)),
            "—"
        );
        assert_eq!(
            format_duration(aurcache_common::api::builds::elapsed_between(
                None,
                Some(100)
            )),
            "—"
        );
    }

    /// Builds are timed by whichever worker ran them, so the two ends can come
    /// from different clocks. A negative duration is nonsense to display.
    #[test]
    fn a_backwards_timestamp_pair_is_not_shown_as_negative() {
        assert_eq!(
            format_duration(aurcache_common::api::builds::elapsed_between(
                Some(500),
                Some(100)
            )),
            "—"
        );
    }

    #[test]
    fn ages_read_in_the_largest_useful_unit() {
        let now = 1_000_000;
        assert_eq!(format_age(Some(now), now), "just now");
        assert_eq!(format_age(Some(now - 59), now), "just now");
        assert_eq!(format_age(Some(now - 60), now), "1m ago");
        assert_eq!(format_age(Some(now - 3599), now), "59m ago");
        assert_eq!(format_age(Some(now - 3600), now), "1h ago");
        assert_eq!(format_age(Some(now - 86400), now), "1d ago");
        assert_eq!(format_age(Some(now - 86400 * 30), now), "30d ago");
    }

    #[test]
    fn a_build_with_no_start_time_has_no_age() {
        assert_eq!(format_age(None, 1_000_000), "—");
    }

    /// A worker whose clock runs ahead stamps a build in the future. "in -3m"
    /// would be worse than rounding to the present.
    #[test]
    fn a_future_timestamp_reads_as_just_now() {
        assert_eq!(format_age(Some(2_000_000), 1_000_000), "just now");
    }
}
