//! What the server reports about itself.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The running server's own version: the release, plus the commit when the
/// build is not exactly that release (see [`crate::version`]).
///
/// The bundled UI shows this rather than its own crate version, so the two
/// never disagree about what is running.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// Whatever the server logs at startup, e.g. `0.5.0+g8afa04a.dirty`.
    pub version: String,
    /// The timezone the server reads cron schedules in. Absent from servers
    /// that predate it, which read them in UTC.
    #[serde(default)]
    pub timezone: Option<Timezone>,
}

/// A timezone as far as a schedule is concerned: what it is called, and where
/// it currently sits against UTC.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Timezone {
    /// The IANA name (`Europe/Paris`), or whatever `TZ` holds. `None` when
    /// only the offset is known -- a container given the host's
    /// `/etc/localtime` as a plain file has the rules but not the name.
    pub name: Option<String>,
    /// Seconds east of UTC, right now: `7200` for CEST.
    pub utc_offset: i32,
}

impl Timezone {
    /// `UTC+02:00`, `UTC-03:30`.
    pub fn offset_text(&self) -> String {
        let sign = if self.utc_offset < 0 { '-' } else { '+' };
        let minutes = self.utc_offset.unsigned_abs() / 60;
        format!("UTC{sign}{:02}:{:02}", minutes / 60, minutes % 60)
    }

    /// `Europe/Paris (UTC+02:00)`, or the offset alone when there is no name.
    pub fn describe(&self) -> String {
        match &self.name {
            Some(name) => format!("{name} ({})", self.offset_text()),
            None => self.offset_text(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Timezone;

    fn zone(name: Option<&str>, utc_offset: i32) -> Timezone {
        Timezone {
            name: name.map(str::to_string),
            utc_offset,
        }
    }

    #[test]
    fn an_offset_reads_as_hours_and_minutes_either_side_of_utc() {
        assert_eq!(zone(None, 0).offset_text(), "UTC+00:00");
        assert_eq!(zone(None, 7200).offset_text(), "UTC+02:00");
        assert_eq!(zone(None, 19800).offset_text(), "UTC+05:30");
        assert_eq!(zone(None, -12600).offset_text(), "UTC-03:30");
    }

    #[test]
    fn a_zone_is_described_by_name_when_it_has_one() {
        assert_eq!(
            zone(Some("Europe/Paris"), 7200).describe(),
            "Europe/Paris (UTC+02:00)"
        );
        assert_eq!(zone(None, -18000).describe(), "UTC-05:00");
    }
}
