//! Registering a worker, and the policy that approves one without an operator.
//!
//! A newly registered worker is `pending` and normally waits for an admin to
//! approve it. Three env-driven modes let common deployments skip that click
//! (see design doc §"One worker command, four enrollment paths"); the fourth
//! path is the default interactive approval, which needs no code here.
//!
//! Every mode below only ever applies to a `pending` worker: see
//! [`EnrollmentPolicy::approves`] for why re-approving a revoked worker must
//! not happen.
//!
//! Modes, highest trust first:
//! 1. **Capability enrollment volume** (`AURCACHE_ENROLLMENT_DIR`): the worker
//!    drops `<fingerprint>.csr` (public only) into a volume the backend mounts
//!    read-only. Presence of that file proves shared-volume access → approve.
//! 2. **Pre-approved fingerprints** (`AURCACHE_PREAPPROVED_WORKERS`): a
//!    comma-separated list of fingerprints.
//! 3. **Shared enrollment token** (`AURCACHE_ENROLLMENT_TOKEN`): fallback for
//!    setups without a shared volume; the worker presents the token.

use crate::worker_policy::worker_policy;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, WorkerReport};
use aurcache_ca::Ca;
use aurcache_common::api::worker::ApprovalStatus;
use aurcache_common::worker::{PROTOCOLS, RegisterRequest};
use aurcache_common::worker_config::SettingDecl;
use aurcache_db::helpers::worker_store;
use sea_orm::DatabaseConnection;
use std::path::PathBuf;
use subtle::ConstantTimeEq;

/// How this server approves workers on its own, read once from the
/// environment.
#[derive(Debug, Clone, Default)]
pub struct EnrollmentPolicy {
    /// `AURCACHE_ENROLLMENT_DIR`: the shared volume a worker proves access to.
    pub dir: Option<PathBuf>,
    /// `AURCACHE_PREAPPROVED_WORKERS`, lowercased.
    pub preapproved: Vec<String>,
    /// `AURCACHE_ENROLLMENT_TOKEN`.
    pub token: Option<String>,
    /// `AURCACHE_EMBEDDED_WORKER_SECRET`: what the hybrid image's own worker
    /// presents. Unset everywhere else -- only that image's entrypoint sets
    /// one.
    pub embedded_secret: Option<String>,
}

impl EnrollmentPolicy {
    /// The policy as this server's environment sets it.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            dir: aurcache_common::server_env::AURCACHE_ENROLLMENT_DIR
                .get_os()
                .map(PathBuf::from),
            preapproved: aurcache_common::server_env::AURCACHE_PREAPPROVED_WORKERS
                .get()
                .map(|raw| parse_preapproved(&raw))
                .unwrap_or_default(),
            token: aurcache_common::server_env::AURCACHE_ENROLLMENT_TOKEN
                .get()
                .ok(),
            embedded_secret: aurcache_common::server_env::AURCACHE_EMBEDDED_WORKER_SECRET
                .get()
                .ok(),
        }
    }

    /// Whether this policy approves the worker with `fingerprint`, presenting
    /// `token`, without an operator.
    ///
    /// `status` is the worker's *current* status, and gates everything else:
    /// only a `pending` worker is eligible. Auto-approval answers "should I
    /// trust a machine I have never seen?"; it must never answer "should I
    /// re-trust a machine an operator explicitly revoked?".
    ///
    /// This matters because every auto-approve mode is durable ambient policy
    /// that revocation cannot touch — the worker's CSR stays in the enrollment
    /// volume, its fingerprint stays in `AURCACHE_PREAPPROVED_WORKERS`, and it
    /// keeps the shared token in its own environment. Workers re-register on
    /// startup, so without this gate revoking a worker would be undone by its
    /// next restart. Authorization is the `workers` row (the certificate itself
    /// proves nothing — see `design/implemented/worker-routing.md`), which
    /// makes any write of `approved` an immediate, complete re-grant.
    ///
    /// The enrollment volume is checked best-effort: a missing or unreadable
    /// directory simply means that mode does not match.
    #[must_use]
    pub fn approves(&self, status: ApprovalStatus, fingerprint: &str, token: Option<&str>) -> bool {
        if status != ApprovalStatus::Pending {
            return false;
        }
        let has_csr = self
            .dir
            .as_ref()
            .is_some_and(|dir| dir.join(format!("{fingerprint}.csr")).is_file());
        has_csr
            || self.preapproved.iter().any(|f| f == fingerprint)
            || secret_matches(token, self.token.as_deref())
    }

    /// Whether `secret` is this server's embedded-worker secret.
    #[must_use]
    pub fn is_embedded(&self, secret: Option<&str>) -> bool {
        secret_matches(secret, self.embedded_secret.as_deref())
    }
}

/// Whether `provided` is the shared secret `expected`.
///
/// In constant time: an early-exit comparison would let a caller find the
/// secret a byte at a time. An unset or empty `expected` matches nothing, so
/// a server that configured no secret accepts none.
#[must_use]
pub fn secret_matches(provided: Option<&str>, expected: Option<&str>) -> bool {
    matches!(
        (provided, expected),
        (Some(p), Some(e)) if !e.is_empty() && bool::from(p.as_bytes().ct_eq(e.as_bytes()))
    )
}

/// Parse `AURCACHE_PREAPPROVED_WORKERS`: comma-separated fingerprints.
///
/// A fingerprint is the lowercase hex SHA-256 of the worker's public key, so
/// it has no separator of its own to split on.
#[must_use]
pub fn parse_preapproved(raw: &str) -> Vec<String> {
    raw.split(',')
        // Case-insensitively, like the worker's own pin: fingerprints are
        // computed lowercase, and one pasted in capitals is the same one.
        .map(|fp| fp.trim().to_ascii_lowercase())
        .filter(|fp| !fp.is_empty())
        .collect()
}

/// What [`register`] made of a registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registered {
    /// Recorded, with its certificate issued and any auto-approval applied.
    Agreed {
        /// The worker's fingerprint, from its CSR.
        fingerprint: String,
        /// The newest worker protocol both sides speak.
        protocol: u32,
    },
    /// The CSR could not be read.
    InvalidCsr(String),
    /// No worker protocol in common. Nothing about the worker was recorded,
    /// so it never reaches the approval list.
    NoCommonProtocol(String),
}

/// Register (or re-register) a worker. Idempotent by CSR fingerprint.
///
/// The CSR is signed at once so the certificate is ready the moment an admin
/// approves; the worker only receives it (and may only use it) once approved.
pub async fn register(
    db: &DatabaseConnection,
    ca: &Ca,
    activity: &ActivityLog,
    policy: &EnrollmentPolicy,
    input: &RegisterRequest,
) -> anyhow::Result<Registered> {
    let fingerprint = match aurcache_ca::fingerprint_from_csr_pem(&input.csr_pem) {
        Ok(fingerprint) => fingerprint,
        Err(e) => return Ok(Registered::InvalidCsr(e.to_string())),
    };

    let Some(protocol) = PROTOCOLS.negotiate(input.protocols) else {
        let reason = format!(
            "this server speaks worker protocol {PROTOCOLS}; worker '{}' speaks {}",
            input.name, input.protocols
        );
        tracing::warn!("Refused to register a worker: {reason}");
        return Ok(Registered::NoCommonProtocol(reason));
    };

    // The hybrid image's embedded worker keeps its identity in an anonymous
    // volume, which an orchestrator that recreates the container discards --
    // TrueNAS does on every start. Its row takes the new key, rather than the
    // same worker enrolling as one more machine each time.
    let embedded = policy.is_embedded(input.embedded_secret.as_deref());
    if embedded
        && let worker_store::Rekey::Rekeyed { previous } =
            worker_store::rekey_embedded(db, &fingerprint).await?
    {
        tracing::info!(
            "The embedded worker came back with a new key ({previous} -> {fingerprint}); \
             its row keeps its history and approval"
        );
    }

    // Asked before registering, because registering is an upsert and would
    // erase the difference: the first time is an enrollment, and every time
    // after is a worker coming back up.
    let known = worker_store::find_worker_by_fingerprint(db, &fingerprint)
        .await?
        .is_some();

    // Stored as JSON, because the server renders a declaration and never
    // reasons about it.
    let declaration = serde_json::to_string(&input.settings).unwrap_or_else(|e| {
        activity.emit(Event::WorkerReportFailed {
            worker: input.name.as_str().into(),
            report: WorkerReport::Declaration,
            error: e.to_string(),
        });
        "[]".to_string()
    });

    let worker = worker_store::register_worker(
        db,
        &worker_store::WorkerRegistration {
            name: &input.name,
            fingerprint: &fingerprint,
            arches: &input.arches,
            version: &input.version,
            kind: &input.kind,
            package_affinity: &input.affinity,
            priority: input.priority,
            // Clamped to at least 1: a worker reporting 0 would be treated as
            // permanently full and could never block a lower-priority worker,
            // silently defeating its own priority.
            concurrency: i32::try_from(input.concurrency.max(1)).unwrap_or(i32::MAX),
            settings_declaration: &declaration,
        },
    )
    .await?;
    if embedded && !worker.embedded {
        worker_store::mark_embedded(db, worker.id).await?;
    }
    if embedded {
        adopt_0_5_limits(db, activity, &worker.name, worker.id, &input.settings).await;
    }

    if known {
        activity.emit(Event::WorkerRegistered {
            worker: worker.name.clone().into(),
            version: Some(input.version.clone()).filter(|v| !v.is_empty()),
        });
    } else {
        activity.emit(Event::WorkerEnrolled {
            worker: worker.name.clone().into(),
        });
    }

    // Issue the leaf certificate, or re-issue one this CA did not sign.
    //
    // The second case is a CA that has been regenerated -- a deployment that
    // lost the directory holding it, most often. Every certificate issued by
    // the old CA is then unverifiable, and a worker presenting one fails mTLS
    // with `BadSignature` on every request; handing the same certificate back
    // on re-registration would leave it unable to recover however many times
    // it restarted.
    let needs_cert = match worker.signed_cert.as_deref() {
        None => true,
        Some(cert) => !ca.issued(cert),
    };
    if needs_cert {
        let signed = ca.sign_worker_csr(&input.csr_pem, worker_policy().cert_validity_days)?;
        worker_store::store_signed_cert(db, worker.id, &signed.cert_pem, signed.not_after)
            .await?
            // The row was read earlier in this request; gone now means a
            // concurrent delete raced registration, which is a server-side
            // inconsistency rather than a bad id.
            .ok_or_else(|| anyhow::anyhow!("worker vanished mid-registration"))?;
    }

    // Non-interactive enrollment. Eligibility (pending only) is enforced by
    // the policy, so a revoked worker is never re-approved by re-registering.
    if policy.approves(
        worker.status,
        &fingerprint,
        input.enrollment_token.as_deref(),
    ) {
        worker_store::approve_worker(db, worker.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("worker vanished mid-registration"))?;
        tracing::info!("Auto-approved worker '{}' ({fingerprint})", worker.name);
        // No user: nobody clicked this. The log says the server did it.
        activity.emit(Event::WorkerApproved {
            worker: worker.name.clone().into(),
        });
    }

    Ok(Registered::Agreed {
        fingerprint,
        protocol,
    })
}

/// Give the embedded worker the build limits an upgrade from 0.5.0 kept.
///
/// Logged rather than failing the registration: a limit lost is worth a line,
/// and a worker that cannot register builds nothing at all.
async fn adopt_0_5_limits(
    db: &DatabaseConnection,
    activity: &ActivityLog,
    name: &str,
    id: i32,
    declared: &[SettingDecl],
) {
    let adopted = match worker_store::adopt_0_5_limits(db, id, declared).await {
        Ok(adopted) => adopted,
        Err(e) => {
            tracing::warn!("Could not give the embedded worker 0.5.0's build limits: {e}");
            return;
        }
    };
    for refused in &adopted.refused {
        tracing::warn!(
            "Dropped 0.5.0's build limit {}={}: {}",
            refused.key,
            refused.value,
            refused.reason
        );
    }
    if !adopted.saved.is_empty() {
        tracing::info!(
            "The embedded worker took over 0.5.0's build limits: {}",
            adopted.saved.set.join(", ")
        );
        activity.emit(Event::WorkerSettingsSaved {
            worker: name.into(),
            set: adopted.saved.set,
            reset: adopted.saved.reset,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PENDING: ApprovalStatus = ApprovalStatus::Pending;

    fn policy() -> EnrollmentPolicy {
        EnrollmentPolicy::default()
    }

    #[test]
    fn enrollment_volume_grants_approval() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fp.csr"), "csr").unwrap();
        let policy = EnrollmentPolicy {
            dir: Some(dir.path().to_path_buf()),
            ..policy()
        };
        assert!(policy.approves(PENDING, "fp", None));
        assert!(!policy.approves(PENDING, "other", None));
    }

    #[test]
    fn preapproved_fingerprint_grants_approval() {
        let policy = EnrollmentPolicy {
            preapproved: vec!["aaa".to_string(), "bbb".to_string()],
            ..policy()
        };
        assert!(policy.approves(PENDING, "bbb", None));
        assert!(!policy.approves(PENDING, "ccc", None));
    }

    #[test]
    fn matching_token_grants_approval() {
        let policy = EnrollmentPolicy {
            token: Some("s3cret".to_string()),
            ..policy()
        };
        assert!(policy.approves(PENDING, "fp", Some("s3cret")));
        assert!(!policy.approves(PENDING, "fp", Some("wrong")));
        // No configured token -> a provided token never approves.
        assert!(!self::policy().approves(PENDING, "fp", Some("s3cret")));
        // Empty configured token is treated as unset.
        let empty = EnrollmentPolicy {
            token: Some(String::new()),
            ..self::policy()
        };
        assert!(!empty.approves(PENDING, "fp", Some("")));
    }

    #[test]
    fn no_mode_matches_stays_pending() {
        assert!(!policy().approves(PENDING, "fp", None));
    }

    /// A revoked worker re-registering must stay revoked even when *every*
    /// auto-approve mode matches. Workers re-register on startup, so without
    /// this a revoke would be undone by the worker's next restart. An approved
    /// one needs no re-approval; skipping the write keeps re-registration
    /// idempotent.
    #[test]
    fn only_a_pending_worker_is_auto_approved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fp.csr"), "csr").unwrap();
        let everything = EnrollmentPolicy {
            dir: Some(dir.path().to_path_buf()),
            preapproved: vec!["fp".to_string()],
            token: Some("s3cret".to_string()),
            embedded_secret: None,
        };
        for status in [ApprovalStatus::Approved, ApprovalStatus::Revoked] {
            assert!(
                !everything.approves(status, "fp", Some("s3cret")),
                "{status} should not be auto-approved"
            );
        }
    }

    #[test]
    fn parse_preapproved_takes_comma_separated_fingerprints() {
        let parsed = parse_preapproved(" AA , bb ,, cc");
        assert_eq!(parsed, vec!["aa", "bb", "cc"]);
        assert!(parse_preapproved("").is_empty());
        assert!(parse_preapproved(" , ").is_empty());
    }

    #[test]
    fn only_the_servers_own_embedded_secret_matches() {
        assert!(secret_matches(Some("s3cret"), Some("s3cret")));
        assert!(!secret_matches(Some("guess"), Some("s3cret")));
        assert!(!secret_matches(None, Some("s3cret")));
        // A server outside the hybrid image has no embedded worker at all.
        assert!(!secret_matches(Some(""), Some("")));
        assert!(!secret_matches(Some("s3cret"), None));
    }
}
