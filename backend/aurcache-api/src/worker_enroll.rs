//! Non-interactive worker enrollment (auto-approval) policy.
//!
//! A newly registered worker is `pending` and normally waits for an admin to
//! approve it. Three env-driven modes let common deployments skip that click
//! (see design doc §"One worker command, four enrollment paths"); the fourth
//! path is the default interactive approval, which needs no code here.
//!
//! Every mode below only ever applies to a `pending` worker: see
//! [`auto_approves`] for why re-approving a revoked worker must not happen.
//!
//! Modes, highest trust first:
//! 1. **Capability enrollment volume** (`AURCACHE_ENROLLMENT_DIR`): the worker
//!    drops `<fingerprint>.csr` (public only) into a volume the backend mounts
//!    read-only. Presence of that file proves shared-volume access → approve.
//! 2. **Pre-approved fingerprints** (`AURCACHE_PREAPPROVED_WORKERS`): a
//!    comma-separated list of `fingerprint[:name[:arches]]` entries.
//! 3. **Shared enrollment token** (`AURCACHE_ENROLLMENT_TOKEN`): fallback for
//!    setups without a shared volume; the worker presents the token.

use aurcache_common::api::worker::ApprovalStatus;
use std::env;
use std::path::PathBuf;
use subtle::ConstantTimeEq;

/// Whether the policy approves this worker without an operator: the pure
/// decision, with the environment and filesystem passed in so it is testable.
///
/// `status` is the worker's *current* status, and gates everything else: only a
/// `pending` worker is eligible. Auto-approval answers "should I trust a machine
/// I have never seen?"; it must never answer "should I re-trust a machine an
/// operator explicitly revoked?".
///
/// This matters because every auto-approve mode below is durable ambient policy
/// that revocation cannot touch — the worker's CSR stays in the enrollment
/// volume, its fingerprint stays in `AURCACHE_PREAPPROVED_WORKERS`, and it keeps
/// the shared token in its own environment. Workers re-register on startup, so
/// without this gate revoking a worker would be undone by its next restart.
/// Authorization is the `workers` row (the certificate itself proves nothing —
/// see `design/implemented/worker-routing.md`), which makes any write of `approved` an
/// immediate, complete re-grant.
#[must_use]
pub fn auto_approves(
    status: ApprovalStatus,
    fingerprint: &str,
    provided_token: Option<&str>,
    expected_token: Option<&str>,
    preapproved_fingerprints: &[String],
    enrollment_dir_has_csr: bool,
) -> bool {
    if status != ApprovalStatus::Pending {
        return false;
    }
    if enrollment_dir_has_csr {
        return true;
    }
    if preapproved_fingerprints.iter().any(|f| f == fingerprint) {
        return true;
    }
    // In constant time: the token is a shared secret, and an early-exit
    // comparison would let a caller find it a byte at a time.
    matches!(
        (provided_token, expected_token),
        (Some(p), Some(e)) if !e.is_empty() && bool::from(p.as_bytes().ct_eq(e.as_bytes()))
    )
}

/// Parse `AURCACHE_PREAPPROVED_WORKERS` (`fp[:name[:arches]],…`) into the set of
/// pre-approved fingerprints (the first `:`-separated field of each entry).
#[must_use]
pub fn parse_preapproved(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|entry| entry.split(':').next())
        // Case-insensitively, like the worker's own pin: fingerprints are
        // computed lowercase, and one pasted in capitals is the same one.
        .map(|fp| fp.trim().to_ascii_lowercase())
        .filter(|fp| !fp.is_empty())
        .collect()
}

/// Whether the auto-approval policy in the environment approves this worker.
///
/// Reads `AURCACHE_ENROLLMENT_DIR`, `AURCACHE_PREAPPROVED_WORKERS`, and
/// `AURCACHE_ENROLLMENT_TOKEN`. Filesystem access for the enrollment volume is
/// best-effort: a missing/unreadable dir simply means that mode doesn't match.
#[must_use]
pub fn env_auto_approves(
    status: ApprovalStatus,
    fingerprint: &str,
    provided_token: Option<&str>,
) -> bool {
    let has_csr = env::var("AURCACHE_ENROLLMENT_DIR").is_ok_and(|dir| {
        PathBuf::from(dir)
            .join(format!("{fingerprint}.csr"))
            .is_file()
    });

    let preapproved = env::var("AURCACHE_PREAPPROVED_WORKERS")
        .map(|raw| parse_preapproved(&raw))
        .unwrap_or_default();

    let expected_token = env::var("AURCACHE_ENROLLMENT_TOKEN").ok();

    auto_approves(
        status,
        fingerprint,
        provided_token,
        expected_token.as_deref(),
        &preapproved,
        has_csr,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PENDING: ApprovalStatus = ApprovalStatus::Pending;

    #[test]
    fn enrollment_volume_grants_approval() {
        assert!(auto_approves(PENDING, "fp", None, None, &[], true));
    }

    #[test]
    fn preapproved_fingerprint_grants_approval() {
        let pre = vec!["aaa".to_string(), "bbb".to_string()];
        assert!(auto_approves(PENDING, "bbb", None, None, &pre, false));
        assert!(!auto_approves(PENDING, "ccc", None, None, &pre, false));
    }

    #[test]
    fn matching_token_grants_approval() {
        assert!(auto_approves(
            PENDING,
            "fp",
            Some("s3cret"),
            Some("s3cret"),
            &[],
            false
        ));
        assert!(!auto_approves(
            PENDING,
            "fp",
            Some("wrong"),
            Some("s3cret"),
            &[],
            false
        ));
        // No configured token -> a provided token never approves.
        assert!(!auto_approves(
            PENDING,
            "fp",
            Some("s3cret"),
            None,
            &[],
            false
        ));
        // Empty configured token is treated as unset.
        assert!(!auto_approves(
            PENDING,
            "fp",
            Some(""),
            Some(""),
            &[],
            false
        ));
    }

    #[test]
    fn no_mode_matches_stays_pending() {
        assert!(!auto_approves(PENDING, "fp", None, None, &[], false));
    }

    /// A revoked worker re-registering must stay revoked even when *every*
    /// auto-approve mode matches. Workers re-register on startup, so without
    /// this a revoke would be undone by the worker's next restart.
    #[test]
    fn revoked_worker_is_never_auto_approved() {
        let pre = vec!["fp".to_string()];
        assert!(!auto_approves(
            ApprovalStatus::Revoked,
            "fp",
            Some("s3cret"),
            Some("s3cret"),
            &pre,
            true
        ));
    }

    /// An already-approved worker needs no re-approval; skipping the write
    /// keeps re-registration idempotent.
    #[test]
    fn approved_worker_is_not_reapproved() {
        assert!(!auto_approves(
            ApprovalStatus::Approved,
            "fp",
            None,
            None,
            &[],
            true
        ));
    }

    /// Only a pending worker can be auto-approved. This used to also cover an
    /// unrecognised status string, which the type no longer permits — a status
    /// is now one of three variants, so "something else" cannot be constructed
    /// to be defaulted into approval.
    #[test]
    fn only_a_pending_worker_is_auto_approved() {
        for status in [ApprovalStatus::Approved, ApprovalStatus::Revoked] {
            assert!(
                !auto_approves(status, "fp", None, None, &[], true),
                "{status} should not be auto-approved"
            );
        }
    }

    #[test]
    fn parse_preapproved_extracts_fingerprints() {
        let parsed = parse_preapproved("aa:worker-a:x86_64, bb , cc:worker-c");
        assert_eq!(parsed, vec!["aa", "bb", "cc"]);
        assert!(parse_preapproved("").is_empty());
    }
}
