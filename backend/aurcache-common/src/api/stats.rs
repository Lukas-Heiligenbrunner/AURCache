use crate::api::builds::BuildSummary;
use crate::api::log::LogEntry;
use crate::api::package::SimplePackage;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct ListStats {
    pub total_builds: u32,
    pub successful_builds: u32,
    pub failed_builds: u32,

    /// The same three over [`RECENT_DAYS`], by start time.
    ///
    /// A server that has been running for a year is described badly by its
    /// lifetime numbers: a run of failures this week disappears into thousands
    /// of old successes, which is exactly when you want to notice it.
    pub recent_builds: u32,
    pub recent_successful: u32,
    pub recent_failed: u32,

    pub avg_build_time: Option<u64>,
    /// Bytes of every artifact in the repository; `None` while any of them
    /// has no recorded size.
    pub repo_size: Option<i64>,

    /// Packages somebody asked for by name.
    ///
    /// Renamed from `total_packages`, which never counted the total: it has
    /// always excluded dependencies, and the name said otherwise.
    pub requested_packages: u32,
    /// Packages present only because something else needs them.
    pub dependency_packages: u32,

    pub total_build_trend: f32,
    pub avg_build_time_trend: f32,
}

/// The window the `recent_*` counts cover.
///
/// A week: long enough to survive a quiet weekend, short enough that a problem
/// starting on Monday is still visible on Friday.
pub const RECENT_DAYS: i64 = 7;

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct GraphDataPoint {
    pub month: i32,
    pub year: i32,
    /// Every build started that month, whatever became of it.
    pub count: i32,
    /// How many of them succeeded. Plotted against `count`, the gap between
    /// the two lines is the failures — which is the thing worth seeing, and
    /// reads better than a failure count on its own.
    pub successful: i32,
}

#[derive(Deserialize, ToSchema, Serialize)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct UserInfo {
    pub username: Option<String>,
    pub has_api_token: bool,
}

/// A personal API token, as regenerating one answers.
#[derive(Deserialize, ToSchema, Serialize, Debug)]
pub struct ApiTokenResponse {
    /// The new token, in plain text. Only ever returned here: the server keeps
    /// its hash.
    pub token: String,
}

/// How many days of successful builds the longest-builds card ranks.
///
/// A window, not all time: an all-time list barely changes and prompts
/// nothing, while a 30-day window surfaces regressions worth looking at.
pub const LONGEST_WINDOW_DAYS: i64 = 30;

/// Every dashboard card in one response, beside the existing `stats`/`graph`.
///
/// Each section is `Option`: `None` when its query failed, so one bad query
/// never blanks the page — the card renders its own inline error and the rest
/// render normally.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq)]
pub struct DashboardView {
    /// Directly requested packages, newest first.
    pub recent_packages: Option<Vec<SimplePackage>>,
    /// Builds, newest first by start time.
    pub recent_builds: Option<Vec<BuildSummary>>,
    /// Packages whose latest build state is failed.
    pub failed: Option<Vec<SimplePackage>>,
    /// Out-of-date packages split into those needing a hand and a count of
    /// those rebuilding on their own.
    pub out_of_date: Option<OutOfDateSlice>,
    /// The stuck queue: its total depth plus the oldest entries.
    pub queue: Option<QueueSlice>,
    /// Log entries at warning severity and worse, newest first.
    pub problems: Option<Vec<LogEntry>>,
    /// Packages by total artifact size, largest first.
    pub largest: Option<Vec<SimplePackage>>,
    /// Longest successful builds in the window, with the previous run beside
    /// each.
    pub longest: Option<Vec<LongBuild>>,
}

/// Out-of-date packages that nothing will rebuild on its own, plus how many
/// others are out of date but already handled (queued or auto-rebuilding).
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct OutOfDateSlice {
    pub needs_hand: Vec<SimplePackage>,
    pub handled: u64,
}

/// The stuck queue: how deep it is in total, and the oldest entries oldest
/// first (capped — the depth counts past the cap).
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct QueueSlice {
    pub depth: u64,
    pub oldest: Vec<BuildSummary>,
}

/// One long build plus its previous successful duration on the same platform.
///
/// `previous_secs` is `None` when there is no earlier successful build on
/// that platform, never `0`.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct LongBuild {
    pub build: BuildSummary,
    pub previous_secs: Option<u64>,
}
