//! This is where the [`Status`] struct and all of its direct dependencies go.
use serde::{Deserialize, Serialize};
use crate::mirror::Mirrors;

/// Raw, typed form of the JSON output given by performing a GET request on [`Status::URL`](Status::URL).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Raw {
    cutoff: u32,
    last_check: String,
    num_checks: u32,
    check_frequency: u32,
    urls: Vec<crate::mirror::Raw>,
    version: u32,
}

/// The status of all the Arch Linux mirrors.
#[derive(Debug, Clone, PartialOrd, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// The cut off.
    pub cutoff: u32,

    /// The last time every listed Arch Linux mirror polled the [`lastsync`] file.
    pub last_check: chrono::DateTime<chrono::Utc>,

    /// The number of checks that have been run in the last 24 hours.
    pub num_checks: u32,

    /// The frequency of each check.
    pub check_frequency: u32,

    /// Every known Arch Linux mirror.
    pub urls: Mirrors,

    /// The version of the status.
    pub version: u32,
}

impl Status {
    /// The URL where the JSON is found from.
    pub const URL: &'static str = "https://archlinux.org/mirrors/status/json";

    /// Get the status from [`Status::URL`](Self::URL).
    pub async fn get_from_default_url() -> reqwest::Result<Self> {
        Self::get_from_url(Self::URL).await
    }

    /// Get the status from a given url.
    pub async fn get_from_url(url: &str) -> reqwest::Result<Self> {
        let response = reqwest::get(url).await?;
        let raw: Raw = response
            .json()
            .await
            .expect("failed to parse response to json");

        Ok(Self::from(raw))
    }
}

impl From<Raw> for Status {
    fn from(raw: Raw) -> Self {
        let last_check: chrono::DateTime<chrono::Utc> = raw
            .last_check
            .parse()
            .expect("failed to parse last_check field from raw status");
        let urls: Vec<crate::Mirror> = raw.urls.into_iter().map(crate::Mirror::from).collect();

        Self {
            cutoff: raw.cutoff,
            last_check,
            num_checks: raw.num_checks,
            check_frequency: raw.check_frequency,
            urls: Mirrors(urls),
            version: raw.version,
        }
    }
}