use crate::settings::SettingSource;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(ToSchema, Deserialize, Serialize, Clone, Debug)]
pub struct SettingValue {
    /// Raw string value to store. Numeric settings should pass the number as a
    /// string. An empty string is a valid value for nullable/optional settings.
    pub value: String,
}

#[derive(ToSchema, Deserialize, Serialize, Clone, Debug)]
pub struct SettingResponse {
    pub value: String,
    pub source: SettingSource,
}

/// When a schedule a setting holds would run, for showing while it is being
/// written. The server works it out, since only it knows its own timezone and
/// what `H` resolves to on this instance.
#[derive(ToSchema, Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SchedulePreview {
    /// The next runs, as Unix seconds, soonest first.
    Runs { at: Vec<i64> },
    /// Valid, but no date ever matches it (`0 0 31 2 *`).
    Never,
    /// Empty: the job is off.
    Disabled,
    /// Not a schedule, and why.
    Invalid { reason: String },
}
