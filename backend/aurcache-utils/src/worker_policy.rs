//! How the server polices remote workers, read once from its environment.
//!
//! In one place because more than one part of the server enforces it: the
//! worker API renews leases and requeues a revoked worker's builds, and the
//! lease reaper reclaims the leases nobody renewed. Each used to read the
//! variables for itself, so the two could disagree about the same setting.
//!
//! Read once: these back per-claim and per-heartbeat paths, and the process
//! environment is fixed at start in every deployment, so a change needs a
//! restart like any other setting.

use aurcache_common::units::parse_duration;
use std::sync::LazyLock;

/// The server's worker tuning; see [`worker_policy`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerPolicy {
    /// How long a lease lasts without a heartbeat, in seconds (`LEASE_TTL`).
    pub lease_ttl_secs: i64,
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
        let duration = |key: &str, default: u64| -> u64 {
            let Some(raw) = lookup(key).filter(|v| !v.trim().is_empty()) else {
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
        let count = |key: &str, default: i64| -> i64 {
            let Some(raw) = lookup(key).filter(|v| !v.trim().is_empty()) else {
                return default;
            };
            raw.trim().parse().unwrap_or_else(|_| {
                tracing::warn!("ignoring {key}={raw:?} (expected a whole number); using {default}");
                default
            })
        };
        let seconds =
            |key: &str, default: u64| i64::try_from(duration(key, default)).unwrap_or(i64::MAX);
        Self {
            lease_ttl_secs: seconds("LEASE_TTL", 60),
            max_attempts: i32::try_from(count("MAX_ATTEMPTS", 3)).unwrap_or(3),
            spill_delay_secs: seconds("WORKER_SPILL_DELAY", 60),
            liveness_timeout_secs: seconds("WORKER_LIVENESS_TIMEOUT", 60),
            reap_interval_secs: duration("REAP_INTERVAL", 20).max(1),
            reap_backstop_grace_secs: seconds("REAP_BACKSTOP_GRACE", 300),
            cert_validity_days: count("WORKER_CERT_VALIDITY_DAYS", 3650),
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
        assert_eq!(p.max_attempts, 3);
        assert_eq!(p.reap_interval_secs, 20);
        assert_eq!(p.cert_validity_days, 3650);
    }

    /// The docs give `LEASE_TTL` as a duration, and the worker reads it as
    /// one; the server reading only bare seconds made `90s` mean the default.
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
}
