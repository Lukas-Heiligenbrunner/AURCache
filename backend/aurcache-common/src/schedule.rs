//! When scheduled jobs run: crontab expressions, with Jenkins's `H`.
//!
//! Five fields, as in crontab(5): `minute hour day-of-month month
//! day-of-week`. Each is `*`, a number, a range `a-b`, a step `*/n`, `a-b/n`
//! or `a/n` (from `a` to the end), or a comma-separated list of those. Months
//! and weekdays take names too (`jan`, `sun`); Sunday is 0 or 7. When both
//! day fields are restricted a day matching *either* runs, as in crontab.
//!
//! `H` stands for one value picked from the field's range by a hash, so that
//! jobs written alike do not all start at the top of the same hour:
//! `H 3 * * *` runs at some minute past 3 every day, the same minute each
//! time. `H(0-29)` picks from a range, `H/15` steps from a picked offset, and
//! `H(0-29)/10` does both. The hash is seeded per instance and per job (see
//! [`job_seed`]), so two servers -- or two jobs on one -- spread apart.
//!
//! The shortcuts are Jenkins's, which use `H` for the same reason: `@hourly`
//! is `H * * * *`, `@daily` `H H * * *`, `@midnight` `H H(0-2) * * *` (some
//! time after midnight, as its name says), `@weekly` `H H * * H`, `@monthly`
//! `H H H * *` and `@yearly` (or `@annually`) `H H H H *`. A hashed day of the month is at most the 28th, so it falls in
//! every month.
//!
//! Times are read in the zone of the `now` handed to
//! [`Schedule::next_after`]; the server passes its local time.

use std::fmt;

/// The schedule that never runs, for saying "off" rather than leaving the
/// setting empty -- which means the same.
pub const NEVER: &str = "@never";

/// Whether `expr` turns its job off: empty, or [`NEVER`].
#[must_use]
pub fn is_off(expr: &str) -> bool {
    let expr = expr.trim();
    expr.is_empty() || expr.eq_ignore_ascii_case(NEVER)
}

/// A parsed schedule, its `H` fields resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    minutes: u64,
    hours: u64,
    /// Bit `d` for day `d`, 1-31.
    days: u64,
    /// Bit `m` for month `m`, 1-12.
    months: u64,
    /// Bit `w` for weekday `w`, 0 (Sunday) to 6.
    weekdays: u64,
    /// Whether the day-of-month field was `*`-based, which crontab reads as
    /// "no restriction" when combining it with the day of the week.
    any_day: bool,
    /// The same for the day-of-week field.
    any_weekday: bool,
}

/// Why an expression is not a schedule; its `Display` says so to the operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleError(String);

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScheduleError {}

/// What one field may hold.
struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    /// The range a bare `H` picks from, when narrower than `min..=max`.
    hashed: (u32, u32),
    names: &'static [&'static str],
}

const MINUTE: Field = Field {
    name: "minute",
    min: 0,
    max: 59,
    hashed: (0, 59),
    names: &[],
};
const HOUR: Field = Field {
    name: "hour",
    min: 0,
    max: 23,
    hashed: (0, 23),
    names: &[],
};
const DAY: Field = Field {
    name: "day of month",
    min: 1,
    max: 31,
    hashed: (1, 28),
    names: &[],
};
const MONTH: Field = Field {
    name: "month",
    min: 1,
    max: 12,
    hashed: (1, 12),
    names: &[
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ],
};
/// 7 is Sunday as well as 0, and folded onto it once parsed.
const WEEKDAY: Field = Field {
    name: "day of week",
    min: 0,
    max: 7,
    hashed: (0, 6),
    names: &["sun", "mon", "tue", "wed", "thu", "fri", "sat"],
};

impl Schedule {
    /// Parse `expr`, resolving each `H` from `seed` (see [`job_seed`]).
    ///
    /// # Errors
    /// When `expr` is not a schedule. An expression in the seconds-first
    /// syntax AURCache used before is refused too, with the same schedule in
    /// this one where it has an equivalent.
    pub fn parse(expr: &str, seed: u64) -> Result<Self, ScheduleError> {
        let expr = expr.trim();
        let expanded = match expr.to_ascii_lowercase().as_str() {
            "@hourly" => "H * * * *",
            "@daily" => "H H * * *",
            "@midnight" => "H H(0-2) * * *",
            "@weekly" => "H H * * H",
            "@monthly" => "H H H * *",
            "@yearly" | "@annually" => "H H H H *",
            _ if expr.starts_with('@') => {
                return Err(ScheduleError(format!("{expr:?} is not a known shortcut")));
            }
            _ => expr,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(Self::wrong_field_count(expr, fields.len()));
        };
        let mut weekdays = parse_field(weekday, &WEEKDAY, mix(seed, 4))?;
        // Sunday is both 0 and 7.
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays & !(1 << 7)) | 1;
        }
        Ok(Self {
            minutes: parse_field(minute, &MINUTE, mix(seed, 0))?,
            hours: parse_field(hour, &HOUR, mix(seed, 1))?,
            days: parse_field(day, &DAY, mix(seed, 2))?,
            months: parse_field(month, &MONTH, mix(seed, 3))?,
            weekdays,
            any_day: day.starts_with('*'),
            any_weekday: weekday.starts_with('*'),
        })
    }

    fn wrong_field_count(expr: &str, count: usize) -> ScheduleError {
        if count == 6 || count == 7 {
            let hint = from_seconds_syntax(expr).map_or_else(
                || " It has no equivalent here, which runs at most once a minute.".to_string(),
                |equivalent| format!(" The same schedule here is `{equivalent}`."),
            );
            ScheduleError(format!(
                "{expr:?} is in the old seconds-first syntax; schedules are now five \
                 fields, as in crontab: minute hour day-of-month month day-of-week.{hint}"
            ))
        } else {
            ScheduleError(format!(
                "{expr:?} has {count} fields; a schedule has five: minute hour \
                 day-of-month month day-of-week"
            ))
        }
    }

    /// The first time strictly after `now` that this schedule runs, in
    /// `now`'s zone; `None` if it never does (`0 0 31 2 *`).
    ///
    /// A time a daylight-saving change skips runs at the same distance past
    /// the change (02:30 on a night that jumps from 02:00 to 03:00 runs at
    /// 03:30). A time it repeats runs once, the first time.
    #[cfg(feature = "clock")]
    #[must_use]
    pub fn next_after(&self, now: &jiff::Zoned) -> Option<jiff::Zoned> {
        // Every combination of month, day and weekday comes round within 28
        // years; a schedule that has not run by then never will.
        const HORIZON_DAYS: i32 = 28 * 366;
        let zone = now.time_zone();
        let mut day = now.date();
        for _ in 0..HORIZON_DAYS {
            if self.runs_on(day) {
                for hour in bits(self.hours) {
                    for minute in bits(self.minutes) {
                        let at = day
                            .at(i8::try_from(hour).ok()?, i8::try_from(minute).ok()?, 0, 0)
                            .to_zoned(zone.clone())
                            .ok()?;
                        if at > *now {
                            return Some(at);
                        }
                    }
                }
            }
            day = day.tomorrow().ok()?;
        }
        None
    }

    #[cfg(feature = "clock")]
    fn runs_on(&self, day: jiff::civil::Date) -> bool {
        if self.months & (1 << day.month()) == 0 {
            return false;
        }
        let by_day = self.days & (1 << day.day()) != 0;
        let by_weekday = self.weekdays & (1 << day.weekday().to_sunday_zero_offset()) != 0;
        match (self.any_day, self.any_weekday) {
            (false, false) => by_day || by_weekday,
            _ => by_day && by_weekday,
        }
    }
}

/// The seed `H` is resolved from, for `job` on the instance `instance`
/// identifies.
///
/// Stable across restarts and releases -- FNV-1a, not `std`'s hasher, whose
/// output is not promised to stay the same -- so a job keeps its minute.
#[must_use]
pub fn job_seed(instance: &str, job: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    instance
        .bytes()
        .chain([0])
        .chain(job.bytes())
        .fold(OFFSET, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(PRIME)
        })
}

/// A different, well-spread value per field from one seed (splitmix64).
fn mix(seed: u64, field: u64) -> u64 {
    let mut z = seed.wrapping_add(field.wrapping_add(1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The set bits of `set`, lowest first.
#[cfg(feature = "clock")]
fn bits(set: u64) -> impl Iterator<Item = u32> {
    (0..64).filter(move |bit| set & (1 << bit) != 0)
}

/// One field, as a bit set over its values.
fn parse_field(text: &str, field: &Field, hash: u64) -> Result<u64, ScheduleError> {
    let mut set = 0;
    for item in text.split(',') {
        set |= parse_item(item, field, hash)
            .ok_or_else(|| ScheduleError(format!("{item:?} is not a valid {}", field.name)))?;
    }
    Ok(set)
}

/// One comma-separated item of a field; `None` if it is not valid there.
fn parse_item(item: &str, field: &Field, hash: u64) -> Option<u64> {
    let (base, step) = match item.split_once('/') {
        Some((base, step)) => (base, Some(step.parse::<u32>().ok().filter(|&s| s > 0)?)),
        None => (item, None),
    };
    let (from, to) = if let Some(hashed) = base.strip_prefix(['H', 'h']) {
        let (low, high) = if hashed.is_empty() {
            field.hashed
        } else {
            let range = hashed.strip_prefix('(')?.strip_suffix(')')?;
            let (low, high) = range.split_once('-')?;
            (value(low, field)?, value(high, field)?)
        };
        if low > high {
            return None;
        }
        // `H` alone is one value; with a step, the offset of the first one.
        let span = step.unwrap_or(high - low + 1);
        let offset = u32::try_from(hash % u64::from(span)).ok()?;
        let first = low + offset;
        match step {
            None => (first, first),
            Some(_) if first > high => return Some(0),
            Some(_) => (first, high),
        }
    } else if base == "*" {
        (field.min, field.max)
    } else if let Some((low, high)) = base.split_once('-') {
        (value(low, field)?, value(high, field)?)
    } else {
        let at = value(base, field)?;
        // `a/n` runs from `a` to the end of the field.
        (at, if step.is_some() { field.max } else { at })
    };
    if from > to {
        return None;
    }
    let step = step.unwrap_or(1);
    Some(
        (from..=to)
            .step_by(usize::try_from(step).ok()?)
            .fold(0, |set, v| set | 1 << v),
    )
}

/// A number or a name, within the field's bounds.
fn value(text: &str, field: &Field) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    let v = match field.names.iter().position(|name| *name == lower) {
        // Month names count from 1, weekday names from 0.
        Some(index) => u32::try_from(index).ok()? + field.min.min(1) * u32::from(field.max == 12),
        None => text.parse().ok()?,
    };
    (field.min..=field.max).contains(&v).then_some(v)
}

/// The same schedule in this syntax, for one written in the seconds-first
/// syntax (`sec min hour day month weekday [year]`) AURCache used before, or
/// `None` when it has no equivalent: anything other than second 0, a
/// restricted year, or something this syntax does not read.
///
/// That syntax counted weekdays from Sunday as 1, so `0 0 2 * * 1` was a
/// Sunday; the numbers are moved down by one, and names are kept. Its `?` is
/// `*`.
#[must_use]
pub fn from_seconds_syntax(expr: &str) -> Option<String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let (second, rest) = fields.split_first()?;
    let (minute, hour, day, month, weekday) = match *rest {
        [minute, hour, day, month, weekday] => (minute, hour, day, month, weekday),
        [minute, hour, day, month, weekday, "*" | "?"] => (minute, hour, day, month, weekday),
        _ => return None,
    };
    if *second != "0" {
        return None;
    }
    let any = |field: &str| {
        if field == "?" {
            "*".to_string()
        } else {
            field.to_string()
        }
    };
    let weekday = if weekday == "?" {
        "*".to_string()
    } else {
        weekday
            .split(',')
            .map(shift_weekday)
            .collect::<Option<Vec<_>>>()?
            .join(",")
    };
    let translated = format!("{minute} {hour} {} {} {weekday}", any(day), any(month));
    Schedule::parse(&translated, 0).ok()?;
    Some(translated)
}

/// One weekday item from Sunday-is-1 numbering to Sunday-is-0; names and the
/// step are kept.
fn shift_weekday(item: &str) -> Option<String> {
    let (base, step) = match item.split_once('/') {
        Some((base, step)) => (base, Some(step)),
        None => (item, None),
    };
    let shift = |part: &str| -> Option<String> {
        match part.parse::<u32>() {
            Ok(n @ 1..=7) => Some((n - 1).to_string()),
            Ok(_) => None,
            Err(_) => Some(part.to_string()),
        }
    };
    let base = if base == "*" {
        base.to_string()
    } else if let Some((low, high)) = base.split_once('-') {
        format!("{}-{}", shift(low)?, shift(high)?)
    } else {
        shift(base)?
    };
    Some(match step {
        Some(step) => format!("{base}/{step}"),
        None => base,
    })
}

#[cfg(all(test, feature = "clock"))]
mod tests {
    use super::*;

    fn at(text: &str) -> jiff::Zoned {
        text.parse().unwrap()
    }

    fn next(expr: &str, now: &str) -> String {
        Schedule::parse(expr, 0)
            .unwrap()
            .next_after(&at(now))
            .unwrap()
            .to_string()
    }

    #[test]
    fn plain_crontab_runs_when_crontab_would() {
        // 03:00 daily, from just before and just after.
        assert_eq!(
            next("0 3 * * *", "2026-10-06T02:59:00+00:00[UTC]"),
            "2026-10-06T03:00:00+00:00[UTC]"
        );
        assert_eq!(
            next("0 3 * * *", "2026-10-06T03:00:00+00:00[UTC]"),
            "2026-10-07T03:00:00+00:00[UTC]"
        );
        // Monday is 1 and Sunday 0 or 7. 2026-10-06 is a Tuesday.
        assert_eq!(
            next("0 2 * * 1", "2026-10-06T12:00:00+00:00[UTC]"),
            "2026-10-12T02:00:00+00:00[UTC]"
        );
        for sunday in ["0", "7", "sun", "SUN"] {
            assert_eq!(
                next(
                    &format!("0 2 * * {sunday}"),
                    "2026-10-06T12:00:00+00:00[UTC]"
                ),
                "2026-10-11T02:00:00+00:00[UTC]"
            );
        }
        // Steps, ranges and lists.
        assert_eq!(
            next("*/15 9-17 * * mon-fri", "2026-10-06T17:46:00+00:00[UTC]"),
            "2026-10-07T09:00:00+00:00[UTC]"
        );
        assert_eq!(
            next("30 4 1,15 * *", "2026-10-06T00:00:00+00:00[UTC]"),
            "2026-10-15T04:30:00+00:00[UTC]"
        );
    }

    /// Both day fields restricted: either one is enough, as in crontab.
    #[test]
    fn restricted_day_and_weekday_run_on_either() {
        // The 13th is a Tuesday in October 2026; Friday the 9th comes first.
        assert_eq!(
            next("0 0 13 * fri", "2026-10-06T12:00:00+00:00[UTC]"),
            "2026-10-09T00:00:00+00:00[UTC]"
        );
        // With the weekday unrestricted, only the day counts.
        assert_eq!(
            next("0 0 13 * *", "2026-10-06T12:00:00+00:00[UTC]"),
            "2026-10-13T00:00:00+00:00[UTC]"
        );
    }

    #[test]
    fn a_schedule_that_never_comes_never_runs() {
        let schedule = Schedule::parse("0 0 31 2 *", 0).unwrap();
        assert_eq!(
            schedule.next_after(&at("2026-10-06T12:00:00+00:00[UTC]")),
            None
        );
        // February 29th does come, if rarely.
        assert_eq!(
            next("0 0 29 2 *", "2026-10-06T12:00:00+00:00[UTC]"),
            "2028-02-29T00:00:00+00:00[UTC]"
        );
    }

    /// Paris skips 02:00-03:00 on 2027-03-28 and repeats it on 2026-10-25.
    #[test]
    fn daylight_saving_changes_skip_nothing_and_repeat_nothing() {
        assert_eq!(
            next("30 2 * * *", "2027-03-28T01:00:00+01:00[Europe/Paris]"),
            "2027-03-28T03:30:00+02:00[Europe/Paris]"
        );
        let schedule = Schedule::parse("30 2 * * *", 0).unwrap();
        let first = schedule
            .next_after(&at("2026-10-25T01:00:00+02:00[Europe/Paris]"))
            .unwrap();
        assert_eq!(first.to_string(), "2026-10-25T02:30:00+02:00[Europe/Paris]");
        assert_eq!(
            schedule.next_after(&first).unwrap().to_string(),
            "2026-10-26T02:30:00+01:00[Europe/Paris]"
        );
    }

    #[test]
    fn h_picks_one_value_per_seed_and_keeps_it() {
        let a = Schedule::parse("H H * * *", job_seed("instance-a", "auto_update")).unwrap();
        assert_eq!(a.minutes.count_ones(), 1);
        assert_eq!(a.hours.count_ones(), 1);
        assert_eq!(
            a,
            Schedule::parse("H H * * *", job_seed("instance-a", "auto_update")).unwrap()
        );
        // Over many instances, the minutes spread out.
        let minutes: std::collections::HashSet<u64> = (0..100)
            .map(|i| {
                Schedule::parse("H 3 * * *", job_seed(&format!("instance-{i}"), "job"))
                    .unwrap()
                    .minutes
            })
            .collect();
        assert!(minutes.len() > 30, "{} distinct minutes", minutes.len());
    }

    #[test]
    fn h_with_a_range_or_a_step_stays_inside_it() {
        for seed in 0..200 {
            let s = Schedule::parse("H(10-14) H/6 H * H", seed).unwrap();
            assert_eq!(s.minutes.count_ones(), 1);
            assert!(bits(s.minutes).all(|m| (10..=14).contains(&m)));
            // Every six hours from an offset under six: four runs a day.
            assert_eq!(s.hours.count_ones(), 4);
            assert!(bits(s.hours).next().unwrap() < 6);
            // A hashed day of the month is in every month.
            assert!(bits(s.days).all(|d| (1..=28).contains(&d)));
            // A hashed weekday is 0-6, never the 7 that means Sunday too.
            assert!(bits(s.weekdays).all(|w| w <= 6));
        }
    }

    /// "Off" can be said out loud, and empty still means it.
    #[test]
    fn never_and_empty_are_off() {
        for off in ["", "  ", "@never", "@NEVER", " @never "] {
            assert!(is_off(off), "{off:?}");
        }
        for on in ["@daily", "H * * * *"] {
            assert!(!is_off(on), "{on:?}");
        }
    }

    #[test]
    fn shortcuts_are_hashed_like_jenkins() {
        let weekly = Schedule::parse("@weekly", 7).unwrap();
        assert_eq!(weekly, Schedule::parse("H H * * H", 7).unwrap());
        assert_eq!(weekly.weekdays.count_ones(), 1);
        // Every hour, at a picked minute.
        let hourly = Schedule::parse("@hourly", 7).unwrap();
        assert_eq!(hourly, Schedule::parse("H * * * *", 7).unwrap());
        assert_eq!(hourly.minutes.count_ones(), 1);
        assert_eq!(hourly.hours.count_ones(), 24);
        // Once a day, within the hours after midnight.
        let midnight = Schedule::parse("@midnight", 7).unwrap();
        assert_eq!(midnight.hours.count_ones(), 1);
        assert!(bits(midnight.hours).next().unwrap() <= 2);
        assert!(Schedule::parse("@fortnightly", 7).is_err());
    }

    #[test]
    fn malformed_fields_are_refused_by_name() {
        for (expr, field) in [
            ("60 * * * *", "minute"),
            ("* 24 * * *", "hour"),
            ("* * 0 * *", "day of month"),
            ("* * * 13 *", "month"),
            ("* * * * 8", "day of week"),
            ("5-1 * * * *", "minute"),
            ("*/0 * * * *", "minute"),
            ("H(5-1) * * * *", "minute"),
        ] {
            let error = Schedule::parse(expr, 0).unwrap_err().to_string();
            assert!(error.contains(field), "{expr}: {error}");
        }
        assert!(
            Schedule::parse("* * * *", 0)
                .unwrap_err()
                .to_string()
                .contains("has 4 fields")
        );
    }

    /// The old syntax is recognised and translated, weekdays renumbered.
    #[test]
    fn the_seconds_syntax_is_translated() {
        assert_eq!(
            from_seconds_syntax("0 0 2 * * 1").as_deref(),
            Some("0 2 * * 0")
        );
        assert_eq!(
            from_seconds_syntax("0 0 1 * * *").as_deref(),
            Some("0 1 * * *")
        );
        assert_eq!(
            from_seconds_syntax("0 30 4 ? * 2-6").as_deref(),
            Some("30 4 * * 1-5")
        );
        assert_eq!(
            from_seconds_syntax("0 0 2 * * Mon,7 *").as_deref(),
            Some("0 2 * * Mon,6")
        );
        // Not second 0, or a year: no equivalent.
        assert_eq!(from_seconds_syntax("*/30 * * * * *"), None);
        assert_eq!(from_seconds_syntax("0 0 2 * * 1 2027"), None);
        // And the parser says so when handed one.
        let error = Schedule::parse("0 0 2 * * 1", 0).unwrap_err().to_string();
        assert!(error.contains("`0 2 * * 0`"), "{error}");
    }
}
