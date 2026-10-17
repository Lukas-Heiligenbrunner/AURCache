//! A build worker, as the operator's view of the fleet sees it.

#[cfg(feature = "db")]
use sea_orm::sea_query::StringLen;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What the workers list shows about one machine.
///
/// Deliberately not the database row. That carries `signed_cert` — the PEM
/// issued at approval — and there is no reason to hand every browser that opens
/// the page a copy of it. The worker gets its own certificate through
/// enrollment, not from this list.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct WorkerSummary {
    pub id: i32,
    /// Operator-facing name reported at enrollment.
    pub name: String,
    pub status: ApprovalStatus,
    /// SHA-256 fingerprint of the worker's certificate: the stable identity
    /// behind the name, which is not unique.
    pub cert_fingerprint: String,
    /// Architectures built natively.
    pub native_arches: Vec<String>,
    /// Architectures built under emulation — slower, and worth telling apart.
    pub emulated_arches: Vec<String>,
    /// Exact pkgbases this worker is provisioned for. A package named by *any*
    /// approved worker may only be built by workers that name it, so this is a
    /// restriction on the package as much as a capability of the worker.
    pub package_affinity: Vec<String>,
    /// Scheduling preference; higher wins. Zero means no preference.
    pub priority: i32,
    /// Unix seconds of the last contact, or `None` if it has never called in.
    pub last_seen: Option<i64>,
    /// Worker software version reported at enrollment or heartbeat.
    pub version: Option<String>,
    /// Which build strategy it runs -- `chroot`, `docker`. `None` for a worker
    /// that enrolled before this was reported.
    ///
    /// Free-form rather than a closed set: the server stores and shows whatever
    /// a worker calls itself, so a new executor needs no change here.
    pub kind: Option<String>,
    /// Whether it has checked in recently enough to be considered connected.
    ///
    /// Derived server-side from `last_seen` and the liveness timeout, because
    /// the timeout is the server's setting — a browser deciding for itself
    /// would call a worker dead on a deployment that allows longer gaps.
    pub online: bool,
    /// Builds it is running right now.
    pub active_builds: i32,
    /// Builds it has finished, by outcome. Together these say whether a worker
    /// is doing the job or merely holding a slot: a machine with a bad
    /// toolchain claims work and fails it, which looks identical to a healthy
    /// one until you count.
    pub successful_builds: i32,
    pub failed_builds: i32,
    /// How many of this worker's settings it refused the configured value for
    /// -- a size that did not parse, a number out of range.
    ///
    /// Carried in the list so a worker running something other than what its
    /// machine was configured with is visible without opening it. `None` until
    /// the worker has reported its configuration, which is not the same answer
    /// as a worker that reports no problems.
    pub settings_rejected: Option<i32>,
    /// Asked to take no new builds and let the ones it holds finish. Still
    /// approved and still heartbeating; resuming it is one click.
    pub paused: bool,
}

/// A worker's configurable surface, as the Workers page shows it.
///
/// Fetched per worker rather than carried in the list: the list is polled while
/// anything is building, and a declaration is a few kilobytes of descriptions
/// that change only when a worker is upgraded.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq)]
pub struct WorkerConfigView {
    pub worker_id: i32,
    /// What this worker declared it accepts, at its last registration.
    pub settings: Vec<crate::worker_config::SettingDecl>,
    /// What those settings resolved to on the machine, as last reported.
    ///
    /// `None` until a worker has been up long enough to send one heartbeat, so
    /// a freshly enrolled worker shows its declaration before its values.
    pub effective: Option<crate::worker_config::EffectiveConfig>,
    /// The values set for this worker here, by key.
    ///
    /// Beside `effective` rather than folded into it: a value saved here is not
    /// necessarily the one running -- the machine may pin that setting, or not
    /// have picked the save up yet -- and the page has to be able to show both.
    /// Includes values for keys the worker no longer declares, which are kept
    /// rather than dropped when a worker is upgraded.
    pub values: std::collections::BTreeMap<String, String>,
    /// The revision of those values as the worker is sent them.
    ///
    /// Compared with `effective.received_revision` to tell a save the worker has
    /// taken from one it has not been reached with yet.
    pub revision: String,
}

/// A save of several of one worker's settings at once.
///
/// One request rather than one per key, because related settings are changed
/// together -- fewer builds each with more memory -- and a worker that picked
/// up half of such a change would run a combination nobody chose. The server
/// writes the whole set in one transaction or none of it.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct WorkerConfigUpdate {
    /// By key: `Some` sets the value, `None` removes it so the worker falls
    /// back to its own environment or built-in default.
    pub settings: std::collections::BTreeMap<String, Option<String>>,
}

/// The deployment-specific pieces the Workers page needs to show a
/// copy-and-run command for enrolling a new worker.
///
/// Only the bits the server actually knows: the image to pull and the port its
/// worker protocol listens on. The host is left to the browser, which fills it
/// from the address the page was opened on — the server has no reliable view of
/// how it is reached from outside.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct WorkerJoinInfo {
    /// Container image a worker runs, e.g.
    /// `ghcr.io/lukas-heiligenbrunner/aurcache-worker:latest`. Overridable with
    /// the `AURCACHE_WORKER_IMAGE` environment variable for private registries
    /// or pinned tags.
    pub image: String,
    /// Port the worker-protocol listener is on (`AURCACHE_WORKER_PORT`,
    /// default 8083).
    pub worker_port: u16,
}

/// Where a worker is in the approval workflow.
///
/// A closed set rather than the string the database holds: the frontend
/// switches on it, and an unrecognised status should be resolved once at the
/// edge rather than re-guessed at every use.
///
/// This is the stored type as well as the wire type: the `workers.status`
/// column maps to it directly, so an unknown value fails at the edge instead of
/// spreading as a string that every reader parses for itself.
#[derive(Deserialize, ToSchema, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(
    feature = "db",
    derive(sea_orm::DeriveActiveEnum, sea_orm::EnumIter),
    sea_orm(rs_type = "String", db_type = "String(StringLen::None)")
)]
pub enum ApprovalStatus {
    /// Approved: holds a signed certificate and may take jobs.
    #[cfg_attr(feature = "db", sea_orm(string_value = "approved"))]
    Approved,
    /// Refused. Its certificate no longer works, and its reservations and
    /// in-flight builds have been released.
    #[cfg_attr(feature = "db", sea_orm(string_value = "revoked"))]
    Revoked,
    /// Enrolled and waiting for an operator. Cannot build.
    ///
    /// Also how a status this build does not know reads, wherever one is
    /// received: a worker that is told something it does not understand keeps
    /// waiting rather than failing to enroll. Last, as serde wants it.
    #[cfg_attr(feature = "db", sea_orm(string_value = "pending"))]
    #[serde(other)]
    Pending,
}

impl ApprovalStatus {
    /// The word this status is stored and sent as.
    ///
    /// One place, so the sea-orm `string_value` attributes and the serde
    /// `rename_all` cannot drift from it unnoticed — the test below holds all
    /// three together.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Revoked => "revoked",
        }
    }

    /// Whether this worker can currently take jobs.
    #[must_use]
    pub const fn can_build(self) -> bool {
        matches!(self, Self::Approved)
    }

    /// Whether it is retired. Revoked rows are kept so build history still
    /// resolves to the machine that produced it, so the list would otherwise
    /// grow without bound.
    #[must_use]
    pub const fn is_retired(self) -> bool {
        matches!(self, Self::Revoked)
    }
}

impl std::fmt::Display for ApprovalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovalStatus;

    /// The wire form, the stored form and `as_str` are the same three words
    /// the column has always held, so existing rows keep their meaning after
    /// the type change — and the three spellings cannot drift apart.
    ///
    /// The sea-orm `string_value` attributes are the fourth, and are checked
    /// by round-tripping a worker through the database in `worker_store`.
    #[test]
    fn every_spelling_of_a_status_agrees() {
        for (status, word) in [
            (ApprovalStatus::Pending, "pending"),
            (ApprovalStatus::Approved, "approved"),
            (ApprovalStatus::Revoked, "revoked"),
        ] {
            assert_eq!(status.as_str(), word);
            assert_eq!(status.to_string(), word);
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{word}\"")
            );
        }
    }

    #[test]
    fn only_approved_workers_can_build() {
        assert!(ApprovalStatus::Approved.can_build());
        assert!(!ApprovalStatus::Pending.can_build());
        assert!(!ApprovalStatus::Revoked.can_build());
        assert!(ApprovalStatus::Revoked.is_retired());
        assert!(!ApprovalStatus::Pending.is_retired());
    }
}
