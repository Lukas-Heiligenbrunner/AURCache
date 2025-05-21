//! This is where the [`Mirror`] struct and all of its dependencies go.
use crate::Country;
use serde::{Deserialize, Serialize};

/// Raw, typed form of the JSON output of each url listed in [`country::Raw`](crate::country::Raw).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Raw {
    url: String,
    protocol: String,
    last_sync: Option<String>,
    completion_pct: f64,
    duration_avg: Option<f64>,
    duration_stddev: Option<f64>,
    score: Option<f64>,
    active: bool,
    country: String,
    country_code: String,
    isos: bool,
    ipv4: bool,
    ipv6: bool,
    details: String,
}

#[derive(Default, Deserialize, Clone, Debug, PartialEq, PartialOrd, Serialize)]
pub struct Mirrors(pub Vec<Mirror>);

/// An Arch Linux mirror and its statistics.
#[derive(Debug, Clone, PartialOrd, PartialEq, Serialize, Deserialize)]
pub struct Mirror {
    /// The url of the mirror.
    pub url: url::Url,

    /// The protocol that this mirror uses.
    pub protocol: crate::Protocol,

    /// The last time it synced from Arch Linux server.
    pub last_sync: Option<chrono::DateTime<chrono::Utc>>,

    /// Completion PCT. Unknown what this means.
    pub completion_pct: f64,

    /// The average duration. Unknown what this means.
    pub duration_average: Option<f64>,

    /// Duration StdDev. Unknown what this means.
    pub duration_stddev: Option<f64>,

    /// The score of the mirror. This is currently calculated as `(hours delay + average duration + standard deviation) / completion percentage`.
    /// Lower is better.
    pub score: Option<f64>,

    /// Whether or not the mirror is active.
    pub active: bool,

    /// The country where the mirror resides in.
    pub country: crate::Country,

    /// Whether or not this mirror has Arch Linux ISOs(?)
    pub isos: bool,

    /// Whether or not this mirror supports IPv4.
    pub ipv4: bool,

    /// Whether or not this mirror supports IPv6.
    pub ipv6: bool,

    /// The details of the mirror.
    pub details: String,
}

impl From<Raw> for Mirror {
    fn from(raw: Raw) -> Self {
        let url: url::Url = raw
            .url
            .parse()
            .expect("failed to parse url field from raw url");
        let protocol: crate::Protocol = raw
            .protocol
            .parse()
            .expect("failed to parse protocol field from raw url");
        let last_sync = raw.last_sync.map(|raw| {
            raw.parse::<chrono::DateTime<chrono::Utc>>()
                .expect("failed to parse last_sync field from raw url")
        });
        let country = Country::new(&raw.country, &raw.country_code);

        Self {
            url,
            protocol,
            last_sync,
            completion_pct: raw.completion_pct,
            duration_average: raw.duration_avg,
            duration_stddev: raw.duration_stddev,
            score: raw.score,
            active: raw.active,
            country,
            isos: raw.isos,
            ipv4: raw.ipv4,
            ipv6: raw.ipv6,
            details: raw.details,
        }
    }
}
