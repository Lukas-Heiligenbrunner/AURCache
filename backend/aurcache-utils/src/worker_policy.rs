//! How the server polices remote workers, read once from its environment.
//!
//! In one place because more than one part of the server enforces it: the
//! worker API renews leases and requeues a revoked worker's builds, and the
//! lease reaper reclaims the leases nobody renewed. Each reading the variables
//! for itself, the two could disagree about the same setting.
//!
//! Read once: these back per-claim and per-heartbeat paths, and the process
//! environment is fixed at start in every deployment, so a change needs a
//! restart like any other setting.

use aurcache_common::server_env::{self, Var};
use aurcache_common::units::parse_duration;
use aurcache_common::worker::Agreement;
use std::sync::LazyLock;

/// The server's worker tuning; see [`worker_policy`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerPolicy {
    /// How long a lease lasts without a heartbeat, in seconds (`LEASE_TTL`).
    pub lease_ttl_secs: i64,
    /// How often workers are to send a heartbeat, in seconds
    /// (`WORKER_HEARTBEAT_INTERVAL`). Like the lease, it reaches a worker in
    /// the [`Agreement`](aurcache_common::worker::Agreement) its registration
    /// is answered with, so the server sets both.
    pub heartbeat_interval_secs: u64,
    /// How many times a build is requeued before it is failed for good
    /// (`MAX_ATTEMPTS`).
    pub max_attempts: i32,
    /// How long a build must sit `ENQUEUED` before worker priority stops
    /// holding it back, in seconds (`WORKER_SPILL_DELAY`).
    ///
    /// A backstop: whether a worker is available is inferred from heartbeats
    /// and lease state, so a worker can look healthy while never actually
    /// claiming (full disk, a bug). This bounds the damage at one delay per job.
    pub spill_delay_secs: i64,
    /// How stale `last_seen` may be before a worker stops counting as
    /// available, in seconds (`WORKER_LIVENESS_TIMEOUT`): about four of the
    /// default 15 s heartbeats.
    pub liveness_timeout_secs: i64,
    /// How often the reaper looks for expired leases, in seconds
    /// (`REAP_INTERVAL`).
    pub reap_interval_secs: u64,
    /// How far past a build's own timeout a still-heartbeating build is
    /// reclaimed anyway, in seconds (`REAP_BACKSTOP_GRACE`).
    pub reap_backstop_grace_secs: i64,
    /// Lifetime of an issued worker certificate, in days
    /// (`WORKER_CERT_VALIDITY_DAYS`).
    ///
    /// Certificates are transport plumbing, not a credential: authorization is
    /// the `workers` row, so holding a valid certificate grants nothing on its
    /// own. There is nothing to gain from a short validity, and no renewal path
    /// exists (a worker reuses its persisted certificate, and the server only
    /// signs when it has none), so a short lifetime would simply brick the
    /// worker. Hence the server certificate's 10 years.
    pub cert_validity_days: i64,
}

impl WorkerPolicy {
    /// The policy `lookup` describes, each variable falling back to its
    /// default when unset or unreadable -- with a warning for the latter, since
    /// `LEASE_TTL=90s` silently meaning 60 is how a typo goes unnoticed.
    ///
    /// Durations take what [`parse_duration`] takes: plain seconds, or `90s`,
    /// `5m`, `1h30m`.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let duration = |key: Var, default: u64| -> u64 {
            let Some(raw) = lookup(key.name()).filter(|v| !v.trim().is_empty()) else {
                return default;
            };
            parse_duration(&raw).unwrap_or_else(|| {
                tracing::warn!(
                    "ignoring {key}={raw:?} (expected seconds, or a duration such as 90s or \
                     5m); using {default}s"
                );
                default
            })
        };
        let count = |key: Var, default: i64| -> i64 {
            let Some(raw) = lookup(key.name()).filter(|v| !v.trim().is_empty()) else {
                return default;
            };
            raw.trim().parse().unwrap_or_else(|_| {
                tracing::warn!("ignoring {key}={raw:?} (expected a whole number); using {default}");
                default
            })
        };
        let seconds =
            |key: Var, default: u64| i64::try_from(duration(key, default)).unwrap_or(i64::MAX);
        let policy = Self {
            lease_ttl_secs: seconds(server_env::LEASE_TTL, 60),
            heartbeat_interval_secs: duration(server_env::WORKER_HEARTBEAT_INTERVAL, 15).max(1),
            max_attempts: i32::try_from(count(server_env::MAX_ATTEMPTS, 3)).unwrap_or(3),
            spill_delay_secs: seconds(server_env::WORKER_SPILL_DELAY, 60),
            liveness_timeout_secs: seconds(server_env::WORKER_LIVENESS_TIMEOUT, 60),
            reap_interval_secs: duration(server_env::REAP_INTERVAL, 20).max(1),
            reap_backstop_grace_secs: seconds(server_env::REAP_BACKSTOP_GRACE, 300),
            cert_validity_days: count(server_env::WORKER_CERT_VALIDITY_DAYS, 3650),
        };
        // One late heartbeat should not cost a build its lease.
        if policy.heartbeat_interval_secs.saturating_mul(2)
            > u64::try_from(policy.lease_ttl_secs).unwrap_or(0)
        {
            tracing::warn!(
                "{}={}s leaves less than two heartbeats per {}={}s: a single late \
                 heartbeat will cost builds their lease",
                server_env::WORKER_HEARTBEAT_INTERVAL,
                policy.heartbeat_interval_secs,
                server_env::LEASE_TTL,
                policy.lease_ttl_secs,
            );
        }
        policy
    }

    /// What a worker that registered speaking `protocol` is told: the
    /// protocol, and this server's lease timing.
    #[must_use]
    pub fn agreement(&self, protocol: u32) -> Agreement {
        Agreement {
            protocol,
            lease_ttl_secs: u64::try_from(self.lease_ttl_secs).unwrap_or(0),
            heartbeat_interval_secs: self.heartbeat_interval_secs,
        }
    }
}

static POLICY: LazyLock<WorkerPolicy> =
    LazyLock::new(|| WorkerPolicy::from_lookup(|key| std::env::var(key).ok()));

/// The server's worker tuning, from its environment.
#[must_use]
pub fn worker_policy() -> &'static WorkerPolicy {
    &POLICY
}

#[cfg(test)]
mod tests {
    use super::WorkerPolicy;
    use std::collections::HashMap;

    fn policy(vars: &[(&str, &str)]) -> WorkerPolicy {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        WorkerPolicy::from_lookup(|key| vars.get(key).cloned())
    }

    #[test]
    fn nothing_set_is_the_defaults() {
        let p = policy(&[]);
        assert_eq!(p.lease_ttl_secs, 60);
        assert_eq!(p.heartbeat_interval_secs, 15);
        assert_eq!(p.max_attempts, 3);
        assert_eq!(p.reap_interval_secs, 20);
        assert_eq!(p.cert_validity_days, 3650);
    }

    /// The docs give `LEASE_TTL` as a duration; reading only bare seconds
    /// made `90s` mean the default.
    #[test]
    fn durations_take_units_and_plain_seconds() {
        let p = policy(&[("LEASE_TTL", "90s"), ("REAP_BACKSTOP_GRACE", " 600 ")]);
        assert_eq!(p.lease_ttl_secs, 90);
        assert_eq!(p.reap_backstop_grace_secs, 600);
        assert_eq!(
            policy(&[("WORKER_SPILL_DELAY", "2m")]).spill_delay_secs,
            120
        );
    }

    #[test]
    fn an_unreadable_value_is_the_default() {
        let p = policy(&[("LEASE_TTL", "soon"), ("MAX_ATTEMPTS", "lots")]);
        assert_eq!(p.lease_ttl_secs, 60);
        assert_eq!(p.max_attempts, 3);
    }

    /// A zero interval would spin the reaper.
    #[test]
    fn the_reap_interval_is_never_zero() {
        assert_eq!(policy(&[("REAP_INTERVAL", "0")]).reap_interval_secs, 1);
    }

    /// Nor the heartbeat: a worker would send them back to back.
    #[test]
    fn the_heartbeat_interval_is_never_zero() {
        assert_eq!(
            policy(&[("WORKER_HEARTBEAT_INTERVAL", "0")]).heartbeat_interval_secs,
            1
        );
    }

    /// The timing a worker is told is the timing the server polices.
    #[test]
    fn the_agreement_carries_the_lease_timing() {
        let agreement =
            policy(&[("LEASE_TTL", "2m"), ("WORKER_HEARTBEAT_INTERVAL", "20s")]).agreement(1);
        assert_eq!(agreement.protocol, 1);
        assert_eq!(agreement.lease_ttl_secs, 120);
        assert_eq!(agreement.heartbeat_interval_secs, 20);
    }
}
