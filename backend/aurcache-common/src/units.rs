//! Sizes and durations written the way people write them in configuration.
//!
//! Shared so every reader agrees: a worker reads `WORKER_BUILD_MEMORY_MAX=32G`
//! from its environment and the server reads `MAX_ARTIFACT_SIZE=40G` from its
//! own, and the same `40G` must mean the same number of bytes to both.
//! Dependency-free, so the browser frontend can use them too.

/// Parse a size such as `450G`, `500MiB`, `20GB` or `1024` into a byte count.
///
/// Read the way coreutils reads a size (`truncate -s`, `dd`): a bare unit
/// letter or an `iB` unit is binary, a `B` unit is decimal. So `450G` and
/// `450GiB` are both 450 x 2^30, `450GB` is 450 x 10^9, and a plain number is
/// bytes. Case, and a space before the unit, do not matter.
///
/// There is no one convention to follow, so this is the rule that surprises
/// the fewest readers. A bare letter is binary to everything configured
/// alongside a worker -- systemd's `MemoryMax=`, Docker's `--memory`, ZFS,
/// sccache -- and is what AURCache's docs have always shown (`20G`). `GB` and
/// `GiB` mean what cargo and ccache say they mean. Only Kubernetes and
/// `numfmt` read a bare `G` as decimal.
///
/// Whole numbers only: a fractional byte count is meaningless and a
/// fractional gigabyte is not worth an ambiguity about rounding.
#[must_use]
pub fn parse_size(raw: impl AsRef<str>) -> Option<u64> {
    let s = raw.as_ref().trim();
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(digits);
    let number: u64 = number.parse().ok()?;

    let unit = unit.trim_start().to_ascii_lowercase();
    // By chars, so a non-ASCII unit can never be sliced mid-character.
    let mut chars = unit.chars();
    let Some(letter) = chars.next() else {
        return Some(number);
    };
    let exponent = match letter {
        'b' if chars.as_str().is_empty() => return Some(number),
        'k' => 1,
        'm' => 2,
        'g' => 3,
        't' => 4,
        _ => return None,
    };
    let base: u64 = match chars.as_str() {
        "" | "ib" => 1024,
        "b" => 1000,
        _ => return None,
    };
    number.checked_mul(base.pow(exponent))
}

/// A byte count as the shortest size [`parse_size`] reads back to exactly it:
/// `21474836480` is `20G`, `1536` is `1536` rather than a rounded `1.5K`.
///
/// For showing a configured size in the field it is edited in, where a value
/// that changed on a round trip through the form would be a change nobody made.
#[must_use]
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(char, u64); 4] = [
        ('T', 1 << 40),
        ('G', 1 << 30),
        ('M', 1 << 20),
        ('K', 1 << 10),
    ];
    UNITS
        .iter()
        .find(|&&(_, unit)| bytes != 0 && bytes.is_multiple_of(unit))
        .map_or_else(
            || bytes.to_string(),
            |&(letter, unit)| format!("{}{letter}", bytes / unit),
        )
}

/// Parse a duration such as `3h`, `15m`, `1h30m`, `30 days` or `900` into
/// seconds.
///
/// A plain number is seconds, which is what every duration setting has always
/// taken, so an existing configuration reads exactly as it did. Otherwise it
/// is one or more `<number><unit>` terms that add up, spaces between them
/// allowed -- the shape systemd reads a time span in (`2h 30min`). Units, in
/// any case: `s`/`sec`/`second`, `m`/`min`/`minute`, `h`/`hr`/`hour`,
/// `d`/`day`, `w`/`week`, each also plural.
///
/// Nothing finer than a second, because nothing here is: every setting it
/// feeds is a whole number of seconds. And no months or years, whose length
/// depends on which one -- `m` is minutes, as it is to systemd and sleep(1).
#[must_use]
pub fn parse_duration(raw: impl AsRef<str>) -> Option<u64> {
    let s = raw.as_ref().trim();
    if let Ok(seconds) = s.parse::<u64>() {
        return Some(seconds);
    }

    let mut rest = s;
    let mut total: u64 = 0;
    let mut terms = 0;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let number: u64 = rest[..digits].parse().ok()?;
        let after = rest[digits..].trim_start();
        // A non-alphabetic char is where the unit ends, so this never slices
        // mid-character.
        let unit_len = after
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(after.len());
        let unit_seconds: u64 = match after[..unit_len].to_ascii_lowercase().as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
            "d" | "day" | "days" => 24 * 60 * 60,
            "w" | "week" | "weeks" => 7 * 24 * 60 * 60,
            // Includes a bare number after a term (`1h30`): minutes or
            // seconds would each be a guess.
            _ => return None,
        };
        total = total.checked_add(number.checked_mul(unit_seconds)?)?;
        terms += 1;
        rest = after[unit_len..].trim_start();
    }
    (terms > 0).then_some(total)
}

/// A number of seconds as the shortest span [`parse_duration`] reads back to
/// exactly it: `10800` is `3h`, `5400` is `90m`, `901` stays `901`.
///
/// The counterpart of [`format_size`], and for the same reason: a duration
/// shown in the field it is edited in must not change by being displayed.
/// Single-term, because `90m` and `1h30m` are the same span and the shorter
/// one is easier to read back.
#[must_use]
pub fn format_duration(seconds: u64) -> String {
    const UNITS: [(char, u64); 4] = [
        ('w', 7 * 24 * 60 * 60),
        ('d', 24 * 60 * 60),
        ('h', 60 * 60),
        ('m', 60),
    ];
    UNITS
        .iter()
        .find(|&&(_, unit)| seconds != 0 && seconds.is_multiple_of(unit))
        .map_or_else(
            || seconds.to_string(),
            |&(letter, unit)| format!("{}{letter}", seconds / unit),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every duration this renders must read back as the same number: that is
    /// the whole contract, and it is what keeps a form from changing a value
    /// nobody edited.
    #[test]
    fn formatted_durations_read_back_exactly() {
        for seconds in [
            0, 1, 59, 60, 90, 900, 3600, 5400, 10800, 86400, 604_800, 901,
        ] {
            let written = format_duration(seconds);
            assert_eq!(parse_duration(&written), Some(seconds), "{written}");
        }
    }

    #[test]
    fn durations_render_in_the_largest_whole_unit() {
        assert_eq!(format_duration(0), "0");
        assert_eq!(format_duration(45), "45");
        assert_eq!(format_duration(900), "15m");
        assert_eq!(format_duration(5400), "90m");
        assert_eq!(format_duration(3 * 3600), "3h");
        assert_eq!(format_duration(30 * 86400), "30d");
    }

    #[test]
    fn a_plain_number_is_seconds() {
        assert_eq!(parse_duration("0"), Some(0));
        assert_eq!(parse_duration("900"), Some(900));
        assert_eq!(parse_duration(" 86400 "), Some(86400));
    }

    #[test]
    fn parses_durations_with_units() {
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("15m"), Some(15 * 60));
        assert_eq!(parse_duration("3h"), Some(3 * 3600));
        assert_eq!(parse_duration("30d"), Some(30 * 86400));
        assert_eq!(parse_duration("2w"), Some(14 * 86400));
        assert_eq!(parse_duration("15min"), Some(15 * 60));
        assert_eq!(parse_duration("3 Hours"), Some(3 * 3600));
        assert_eq!(parse_duration("1 day"), Some(86400));
    }

    #[test]
    fn terms_add_up() {
        assert_eq!(parse_duration("1h30m"), Some(5400));
        assert_eq!(parse_duration("2h 30min"), Some(9000));
        assert_eq!(parse_duration("1d 2h 3m 4s"), Some(86400 + 7200 + 180 + 4));
    }

    #[test]
    fn rejects_durations_it_cannot_read_exactly() {
        for bad in [
            "", "h", "3x", "1h30", "-3h", "1.5h", "3 months", "3y", "3hé", "3é", "h3", "3h,",
        ] {
            assert_eq!(parse_duration(bad), None, "{bad:?}");
        }
    }

    /// An overflow is a value that does not fit, not a wrapped-around small one.
    #[test]
    fn rejects_durations_that_overflow() {
        assert_eq!(parse_duration("18446744073709551615s"), Some(u64::MAX));
        assert_eq!(parse_duration("18446744073709551615s 1s"), None);
        assert_eq!(
            parse_duration("30500568904943w"),
            Some(30_500_568_904_943 * 604_800)
        );
        assert_eq!(parse_duration("30500568904944w"), None);
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1024B"), Some(1024));
        assert_eq!(parse_size("20G"), Some(20 * GIB));
        assert_eq!(parse_size("500m"), Some(500 * 1024 * 1024));
        assert_eq!(parse_size("2T"), Some(2 * 1024 * GIB));
        assert_eq!(parse_size("8k"), Some(8 * 1024));
    }

    #[test]
    fn a_size_is_written_as_what_reads_back_to_it() {
        assert_eq!(format_size(20 * GIB), "20G");
        assert_eq!(format_size(512 * 1024 * 1024), "512M");
        assert_eq!(format_size(3 << 40), "3T");
        assert_eq!(format_size(1536), "1536");
        assert_eq!(format_size(1024), "1K");
        assert_eq!(format_size(0), "0");
        for bytes in [0, 1, 1023, 1024, 1536, 20 * GIB, 20 * GIB + 1, u64::MAX] {
            assert_eq!(parse_size(format_size(bytes)), Some(bytes), "{bytes}");
        }
    }

    /// The coreutils rule: a bare letter and `iB` are binary, `B` is decimal.
    #[test]
    fn a_b_unit_is_decimal_and_the_rest_are_binary() {
        assert_eq!(parse_size("450G"), Some(450 * GIB));
        assert_eq!(parse_size("450GiB"), Some(450 * GIB));
        assert_eq!(parse_size("450GB"), Some(450_000_000_000));
        assert_eq!(parse_size("5kB"), Some(5_000));
        assert_eq!(parse_size("5KiB"), Some(5 * 1024));
        assert_eq!(parse_size("1TB"), Some(1_000_000_000_000));
    }

    #[test]
    fn ignores_case_and_a_space_before_the_unit() {
        assert_eq!(parse_size("450 GiB"), Some(450 * GIB));
        assert_eq!(parse_size(" 450gib "), Some(450 * GIB));
        assert_eq!(parse_size("450 g"), Some(450 * GIB));
    }

    #[test]
    fn rejects_sizes_it_cannot_read_exactly() {
        for bad in [
            "", "abc", "G", "-5G", "1.5G", "450 G B", "450GIBS", "450P", "4 50G", "450Gé", "450é",
        ] {
            assert_eq!(parse_size(bad), None, "{bad:?}");
        }
    }

    /// An overflow is a value that does not fit, not a wrapped-around small one.
    #[test]
    fn rejects_sizes_that_overflow() {
        assert_eq!(parse_size("17179869184G"), None);
        assert_eq!(parse_size("16777216T"), None);
        assert_eq!(parse_size("16777215T"), Some(16_777_215 * 1024 * GIB));
        assert_eq!(parse_size("18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_size("18446744073709551616"), None);
    }
}
