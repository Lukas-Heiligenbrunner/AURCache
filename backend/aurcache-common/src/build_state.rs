//! Build states, triggers and end reasons: what the `builds` and `packages`
//! columns hold and the API sends.
//!
//! Each is stored and sent as its `i32` discriminant, so the schema and the
//! wire format are plain integers, while every consumer matches on the enum.
//! The sea-orm derives are behind the `db` feature, so a browser gets the same
//! types without a database driver.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Serde and schema impls writing a type as its `i32` discriminant, through
/// its `as_i32` and `from_i32`.
///
/// An unknown number is a deserialization error rather than a guess: a value
/// from a newer server is refused instead of being shown as something it is
/// not.
macro_rules! as_integer {
    ($($t:ty => $what:literal),* $(,)?) => {$(
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_i32(self.as_i32())
            }
        }

        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let value = i32::deserialize(d)?;
                Self::from_i32(value).ok_or_else(|| {
                    serde::de::Error::custom(format!("{value} is not a {}", $what))
                })
            }
        }

        impl utoipa::PartialSchema for $t {
            fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
                use utoipa::openapi::schema::{ObjectBuilder, Type};
                ObjectBuilder::new()
                    .schema_type(Type::Integer)
                    .description(Some(concat!("A ", $what, ", as its number.")))
                    .into()
            }
        }

        impl utoipa::ToSchema for $t {}
    )*};
}

as_integer! {
    BuildState => "build state",
    BuildTrigger => "build trigger",
    EndReason => "end reason",
}

/// The state of a build -- and of a package, which shows its latest build's.
///
/// Matched exhaustively wherever it is handled: a new state is a compile
/// error in every caller that has to decide about it, rather than a number
/// nobody recognises silently falling into an "unknown" arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "db",
    derive(sea_orm::DeriveActiveEnum, sea_orm::EnumIter),
    sea_orm(rs_type = "i32", db_type = "Integer")
)]
#[repr(i32)]
pub enum BuildState {
    /// Currently building.
    #[cfg_attr(feature = "db", sea_orm(num_value = 0))]
    Active = 0,
    /// Built successfully.
    #[cfg_attr(feature = "db", sea_orm(num_value = 1))]
    Successful = 1,
    /// The build failed.
    #[cfg_attr(feature = "db", sea_orm(num_value = 2))]
    Failed = 2,
    /// Waiting for a worker to claim it.
    #[cfg_attr(feature = "db", sea_orm(num_value = 3))]
    Enqueued = 3,
    /// Queued, but cannot start yet: one or more dependency builds have not
    /// completed successfully.
    #[cfg_attr(feature = "db", sea_orm(num_value = 4))]
    WaitingForDeps = 4,
    /// Built: the worker handed its artifacts over and is done with it, and
    /// the server is putting them in the repository.
    ///
    /// Its own state rather than more of `Active`, because what changes is who
    /// is responsible. `Active` is a worker holding a lease, and everything that
    /// polices leases -- the heartbeat, the reaper, revoking a worker, claim
    /// capacity -- finds builds by that state. A build being published has no
    /// lease and no worker left to lose it.
    #[cfg_attr(feature = "db", sea_orm(num_value = 5))]
    Publishing = 5,
}

impl BuildState {
    /// The database/wire representation.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Parse the database/wire representation.
    ///
    /// Returns `None` for an unrecognised value rather than guessing, so a
    /// forward-incompatible state from a newer server is visible instead of
    /// being silently rendered as something it is not.
    #[must_use]
    pub const fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Active),
            1 => Some(Self::Successful),
            2 => Some(Self::Failed),
            3 => Some(Self::Enqueued),
            4 => Some(Self::WaitingForDeps),
            5 => Some(Self::Publishing),
            _ => None,
        }
    }

    /// Every state [`Self::is_in_progress`] holds for: a build that has not
    /// settled. A package has at most one such build per platform.
    pub const IN_PROGRESS: [Self; 4] = [
        Self::Active,
        Self::Enqueued,
        Self::WaitingForDeps,
        Self::Publishing,
    ];

    /// Every state, in discriminant order.
    pub const ALL: [Self; 6] = [
        Self::Active,
        Self::Successful,
        Self::Failed,
        Self::Enqueued,
        Self::WaitingForDeps,
        Self::Publishing,
    ];

    /// The state's name where one is written by hand: a filter on the builds
    /// list (`?status=active,publishing`), a CLI flag. Stable, lowercase and
    /// free of spaces, unlike a label meant for reading.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Successful => "successful",
            Self::Failed => "failed",
            Self::Enqueued => "enqueued",
            Self::WaitingForDeps => "waiting-for-deps",
            Self::Publishing => "publishing",
        }
    }

    /// The state as a person reads it, in a list or on a badge: lowercase,
    /// and worded for a build (`building`, `waiting for deps`).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Active => "building",
            Self::Successful => "successful",
            Self::Failed => "failed",
            Self::Enqueued => "enqueued",
            Self::WaitingForDeps => "waiting for deps",
            Self::Publishing => "publishing",
        }
    }

    /// The state a [`Self::key`] names.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.key() == key)
    }

    /// Parse a comma-separated list of keys, as the builds list takes them.
    ///
    /// # Errors
    /// A message naming the key that is not a state, and the ones that are.
    pub fn parse_keys(list: &str) -> Result<Vec<Self>, String> {
        list.split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(|key| {
                Self::from_key(key).ok_or_else(|| {
                    let known: Vec<_> = Self::ALL.iter().map(|s| s.key()).collect();
                    format!(
                        "{key:?} is not a build state (one of: {})",
                        known.join(", ")
                    )
                })
            })
            .collect()
    }

    /// Whether the build has not settled yet — it is running, queued, or held
    /// behind a dependency. `Successful` and `Failed` are the terminal states.
    ///
    /// The UI uses this to decide how eagerly to re-poll: a page showing a
    /// build in one of these states is a page worth refreshing briskly.
    #[must_use]
    pub const fn is_in_progress(self) -> bool {
        matches!(
            self,
            Self::Active | Self::Enqueued | Self::WaitingForDeps | Self::Publishing
        )
    }
}

/// Why a build row was created.
///
/// The retry budget for builds the reaper abandons is *derived* from this column —
/// the count of consecutive `TimeoutRetry` rows — rather than tracked in a
/// mutable counter, so the reason a build exists has to live somewhere a build
/// history can walk, and the rows themselves are the only history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "db",
    derive(sea_orm::DeriveActiveEnum, sea_orm::EnumIter),
    sea_orm(rs_type = "i32", db_type = "Integer")
)]
#[repr(i32)]
pub enum BuildTrigger {
    /// Operator add or explicit rebuild (button, CLI).
    #[cfg_attr(feature = "db", sea_orm(num_value = 0))]
    User = 0,
    /// A version check found the package outdated and requeued it.
    #[cfg_attr(feature = "db", sea_orm(num_value = 1))]
    AutoUpdate = 1,
    /// Automatic retry of a build the server abandoned.
    #[cfg_attr(feature = "db", sea_orm(num_value = 2))]
    TimeoutRetry = 2,
}

impl BuildTrigger {
    /// The database/wire representation.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Parse the database/wire representation.
    ///
    /// Returns `None` for an unrecognised value rather than guessing, so an
    /// unknown trigger from a newer server is visible instead of silently
    /// counting as a user request.
    #[must_use]
    pub const fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::User),
            1 => Some(Self::AutoUpdate),
            2 => Some(Self::TimeoutRetry),
            _ => None,
        }
    }
}

/// Why a build stopped, when the stop was decided by the server rather than
/// reported by the worker.
///
/// `None` on the row means the worker reported a terminal outcome and its
/// `CompleteReport.reason` text is the record; these codes cover the cases only
/// the server can produce: a manual cancel, and the ways a build is abandoned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "db",
    derive(sea_orm::DeriveActiveEnum, sea_orm::EnumIter),
    sea_orm(rs_type = "i32", db_type = "Integer")
)]
#[repr(i32)]
pub enum EndReason {
    /// An operator cancelled the build (`aurcache_utils::cancel`).
    #[cfg_attr(feature = "db", sea_orm(num_value = 0))]
    Canceled = 0,
    /// The owning worker went silent past its lease.
    #[cfg_attr(feature = "db", sea_orm(num_value = 1))]
    LeaseExpired = 1,
    /// The build ran past the backstop deadline while still heartbeating.
    #[cfg_attr(feature = "db", sea_orm(num_value = 2))]
    MaxDuration = 2,
    /// The owning worker kept heartbeating but stopped listing the build: it
    /// lost it -- restarted, or the build process died -- without saying so.
    #[cfg_attr(feature = "db", sea_orm(num_value = 3))]
    Dropped = 3,
    /// An operator revoked the worker that was running it.
    #[cfg_attr(feature = "db", sea_orm(num_value = 4))]
    WorkerRevoked = 4,
    /// Its worker reported that it could not see the build through, for
    /// reasons of its own (`BuildOutcome::Interrupted`).
    #[cfg_attr(feature = "db", sea_orm(num_value = 5))]
    Interrupted = 5,
}

impl EndReason {
    /// The database/wire representation.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Parse the database/wire representation.
    #[must_use]
    pub const fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Canceled),
            1 => Some(Self::LeaseExpired),
            2 => Some(Self::MaxDuration),
            3 => Some(Self::Dropped),
            4 => Some(Self::WorkerRevoked),
            5 => Some(Self::Interrupted),
            _ => None,
        }
    }

    /// Whether an abandonment for this reason counts against the build's retry
    /// budget.
    ///
    /// The build's own doing -- a worker that went silent, a build that would
    /// not end, one lost on the worker -- repeats if retried forever, so it is
    /// counted. So is one its worker could not finish: a worker that breaks
    /// the same way each time must not retry it for ever either. A revocation is the operator's decision about the machine, not
    /// about the build, and a cancel is not retried at all.
    #[must_use]
    pub const fn spends_retry(self) -> bool {
        matches!(
            self,
            Self::LeaseExpired | Self::MaxDuration | Self::Dropped | Self::Interrupted
        )
    }

    /// Why the build was abandoned, as its log's last line says it.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::Canceled => "cancelled by an operator",
            Self::LeaseExpired => "its worker stopped heartbeating",
            Self::MaxDuration => "it exceeded its maximum duration",
            Self::Dropped => "its worker stopped reporting it",
            Self::WorkerRevoked => "its worker was revoked",
            Self::Interrupted => "its worker could not finish it",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each is written as its number, and reads back as itself.
    #[test]
    fn values_travel_as_their_numbers() {
        for state in BuildState::ALL {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, state.as_i32().to_string());
            assert_eq!(serde_json::from_str::<BuildState>(&json).unwrap(), state);
        }
        assert_eq!(
            serde_json::to_string(&EndReason::Dropped).unwrap(),
            "3",
            "the wire form is the stored number"
        );
        assert!(serde_json::from_str::<BuildState>("99").is_err());
        assert!(serde_json::from_str::<BuildTrigger>("\"user\"").is_err());
    }

    /// Keys are what people type, so every state has one, they round-trip,
    /// and a list of them parses with or without spaces.
    #[test]
    fn keys_round_trip_and_lists_of_them_parse() {
        for state in BuildState::ALL {
            assert_eq!(BuildState::from_key(state.key()), Some(state));
            assert!(!state.key().contains(' '), "{state:?}");
        }
        assert_eq!(
            BuildState::parse_keys("active, publishing"),
            Ok(vec![BuildState::Active, BuildState::Publishing])
        );
        assert_eq!(BuildState::parse_keys(""), Ok(vec![]));
        let error = BuildState::parse_keys("active,running").unwrap_err();
        assert!(error.contains("\"running\""), "{error}");
        assert!(error.contains("waiting-for-deps"), "{error}");
    }

    #[test]
    fn in_progress_lists_exactly_the_unsettled_states() {
        for state in BuildState::ALL {
            assert_eq!(
                BuildState::IN_PROGRESS.contains(&state),
                state.is_in_progress(),
                "{state:?}"
            );
        }
    }

    #[test]
    fn an_unknown_value_is_not_guessed() {
        assert_eq!(BuildState::from_i32(99), None);
        assert_eq!(BuildState::from_i32(-1), None);
    }

    /// Triggers and end reasons read back from their numbers, and an unknown
    /// number is not guessed (which would silently count it as `user`).
    #[test]
    fn triggers_and_end_reasons_round_trip() {
        for trigger in [
            BuildTrigger::User,
            BuildTrigger::AutoUpdate,
            BuildTrigger::TimeoutRetry,
        ] {
            assert_eq!(BuildTrigger::from_i32(trigger.as_i32()), Some(trigger));
        }
        assert_eq!(BuildTrigger::from_i32(99), None);
        for reason in [
            EndReason::Canceled,
            EndReason::LeaseExpired,
            EndReason::MaxDuration,
            EndReason::Dropped,
            EndReason::WorkerRevoked,
            EndReason::Interrupted,
        ] {
            assert_eq!(EndReason::from_i32(reason.as_i32()), Some(reason));
        }
        assert_eq!(EndReason::from_i32(99), None);
    }
}
