//! Why a queued build is not being picked up.
//!
//! Computed by the scheduler but shown in the API, so it lives with the other
//! shared shapes rather than in the database layer.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitingReason {
    /// Reserved by package affinity to workers that are not currently live.
    /// Revoking a listed worker releases the reservation.
    Affinity { workers: Vec<String> },
    /// No approved worker builds this architecture, natively or emulated.
    Arch { arch: String },
    /// A capable worker exists but none has been seen recently.
    Offline,
    /// Every capable worker that is up has been asked to pause. They take no
    /// new builds until resumed; resuming one of these picks the build up.
    Paused { workers: Vec<String> },
}

impl std::fmt::Display for WaitingReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Affinity { workers } => {
                write!(f, "reserved for {} (offline)", workers.join(", "))
            }
            Self::Arch { arch } => write!(f, "no worker builds {arch}"),
            Self::Offline => write!(f, "all capable workers are offline"),
            Self::Paused { workers } => {
                write!(
                    f,
                    "intake is stopped on every capable worker ({})",
                    workers.join(", ")
                )
            }
        }
    }
}
