use std::str::FromStr;

pub trait ParseSetting: Sized + Clone {
    fn parse_setting(s: &str) -> Result<Self, String>;
}
/// A span of time written as a duration (`1h`, `90m`, `1h30m`, or plain
/// seconds), held as seconds.
///
/// Its own type for the reason [`ByteSize`] is: a plain number parse would
/// read `1h` as garbage and quietly fall back to the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seconds(pub u64);

impl ParseSetting for Seconds {
    fn parse_setting(s: &str) -> Result<Self, String> {
        aurcache_common::units::parse_duration(s)
            .map(Seconds)
            .ok_or_else(|| format!("expected a duration such as 1h or 90m, got {s:?}"))
    }
}

/// A byte count written as a size (`20G`, `512M`, `1024`).
///
/// Its own type rather than `u64`, whose plain number parse would read `20G`
/// as garbage and quietly fall back to the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl ParseSetting for ByteSize {
    fn parse_setting(s: &str) -> Result<Self, String> {
        aurcache_common::units::parse_size(s)
            .map(ByteSize)
            .ok_or_else(|| format!("expected a size such as 20G, got {s:?}"))
    }
}

impl ParseSetting for String {
    fn parse_setting(s: &str) -> Result<Self, String> {
        Ok(s.to_string())
    }
}

impl ParseSetting for bool {
    /// Accepts the spellings that turn up in environment variables, not just
    /// Rust's `true`/`false` — `BUILD_ON_NEW_VERSION=1` should work.
    fn parse_setting(s: &str) -> Result<Self, String> {
        aurcache_common::settings::parse_boolean(s)
    }
}

impl<T> ParseSetting for Option<T>
where
    T: FromStr + Clone,
    <T as FromStr>::Err: std::fmt::Display,
{
    fn parse_setting(s: &str) -> Result<Self, String> {
        if s.trim().is_empty() {
            Ok(None)
        } else {
            s.parse::<T>().map(Some).map_err(|e| e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ParseSetting, Seconds};

    /// A duration setting reads what the worker's do: units, or plain seconds
    /// as it always took.
    #[test]
    fn durations_take_units_and_plain_seconds() {
        assert_eq!(Seconds::parse_setting("1h"), Ok(Seconds(3600)));
        assert_eq!(Seconds::parse_setting("90m"), Ok(Seconds(5400)));
        assert_eq!(Seconds::parse_setting("3600"), Ok(Seconds(3600)));
        assert!(Seconds::parse_setting("an hour").is_err());
    }

    /// Settings can come from environment variables, where `1` and `yes` are
    /// as idiomatic as `true`. Rejecting them would silently disable a feature
    /// someone believes they turned on.
    #[test]
    fn booleans_accept_the_spellings_people_actually_write() {
        for on in ["true", "TRUE", "1", "yes", "on", " true "] {
            assert_eq!(bool::parse_setting(on), Ok(true), "{on:?}");
        }
        for off in ["false", "0", "no", "off", ""] {
            assert_eq!(bool::parse_setting(off), Ok(false), "{off:?}");
        }
    }

    /// A typo must not quietly read as `false` — that is the failure mode
    /// where someone thinks a feature is on and it never runs.
    #[test]
    fn an_unrecognised_boolean_is_an_error() {
        assert!(bool::parse_setting("ture").is_err());
        assert!(bool::parse_setting("enabled").is_err());
    }
}
