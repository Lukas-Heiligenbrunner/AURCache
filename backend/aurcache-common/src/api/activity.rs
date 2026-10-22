//! How much attention a log entry deserves.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// How much attention an entry deserves.
///
/// A property of *what happened*: each event's kind decides it, so two events
/// of one kind cannot differ in severity -- "publishing failed" and "package
/// added" are not one event with a field. Stored as the number its variants are ordered by, so "this severity and
/// worse" is `severity >= n` -- one indexable comparison rather than a list of
/// kinds the query would have to know.
#[derive(Deserialize, ToSchema, Serialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "db",
    derive(sea_orm::DeriveActiveEnum, sea_orm::EnumIter),
    sea_orm(rs_type = "i32", db_type = "Integer")
)]
pub enum Severity {
    /// Something happened. Most of the log.
    #[cfg_attr(feature = "db", sea_orm(num_value = 0))]
    Info,
    /// Something did not work and the server carried on. Worth a look.
    #[cfg_attr(feature = "db", sea_orm(num_value = 1))]
    Warning,
    /// Something did not work and left the instance worse off.
    #[cfg_attr(feature = "db", sea_orm(num_value = 2))]
    Error,
}

impl Severity {
    /// The word this severity is written as, for a URL or a filter.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// Read a severity back from [`Self::slug`].
    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        [Self::Info, Self::Warning, Self::Error]
            .into_iter()
            .find(|severity| severity.slug() == slug)
    }
}

/// An entry that predates severities is ordinary news.
impl Default for Severity {
    fn default() -> Self {
        Self::Info
    }
}

#[cfg(test)]
mod tests {
    use super::Severity;

    #[test]
    fn severities_round_trip_through_their_slug() {
        for severity in [Severity::Info, Severity::Warning, Severity::Error] {
            assert_eq!(Severity::from_slug(severity.slug()), Some(severity));
        }
        assert_eq!(Severity::from_slug("catastrophe"), None);
    }
}
