//! Enrollment lifecycle: pin the server CA, submit a CSR, wait for approval,
//! persist the signed certificate, and hand back an authenticated client.

use anyhow::{Context, Result};
use aurcache_common::api::worker::ApprovalStatus;
use aurcache_common::worker::{PROTOCOLS, RegisterRequest, RegisterStatus};
use std::time::Duration;

use crate::client::{Incompatible, WorkerClient, fetch_and_pin_ca};
use crate::config::CoreConfig;
use crate::identity::Identity;

/// Drop the CSR into the shared enrollment volume so a co-located backend can
/// auto-approve this worker (bundled single-host topology). Best-effort.
pub fn publish_csr_to_enrollment_dir(cfg: &CoreConfig, fingerprint: &str, csr_pem: &str) {
    let Some(dir) = &cfg.enrollment_dir else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::debug!("enrollment dir {} unavailable: {e}", dir.display());
        return;
    }
    let path = dir.join(format!("{fingerprint}.csr"));
    match std::fs::write(&path, csr_pem) {
        Ok(()) => tracing::info!("Published CSR for auto-approval at {}", path.display()),
        Err(e) => tracing::debug!("could not write {}: {e}", path.display()),
    }
}

/// Assemble the registration payload from the current configuration.
///
/// Registration is how a worker reports its configuration, and *all* of it can
/// have changed since the machine last booted -- or since the server last
/// delivered values -- so this is rebuilt from `cfg` every time rather than
/// cached.
pub fn register_request(cfg: &CoreConfig, csr_pem: String, kind: &str) -> RegisterRequest {
    RegisterRequest {
        protocols: PROTOCOLS,
        name: cfg.name.clone(),
        native_arches: cfg.native_arches.clone(),
        emulated_arches: cfg.emulated_arches.clone(),
        version: aurcache_common::version::full_version(env!("CARGO_PKG_VERSION")),
        kind: kind.to_string(),
        csr_pem,
        enrollment_token: cfg.enrollment_token.clone(),
        embedded_secret: cfg.embedded_secret.clone(),
        affinity: cfg.packages.clone(),
        priority: cfg.priority,
        concurrency: cfg.concurrency as u32,
        settings: cfg.settings.declare(),
    }
}

/// The server has revoked this worker: the one enrollment failure that waiting
/// does not fix.
#[derive(Debug)]
pub struct Revoked;

impl std::fmt::Display for Revoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this worker was revoked by the server; approve it again to resume")
    }
}

impl std::error::Error for Revoked {}

/// How long a worker refused for its protocol waits before registering again.
///
/// Slow, because only an upgrade on one side or the other changes the answer.
/// Not an exit, because systemd would only restart the worker into the same
/// refusal.
const REFUSED_RETRY: Duration = Duration::from_secs(300);

/// [`ensure_enrolled`], retried every poll interval until it succeeds.
///
/// The server may not be reachable yet -- still starting in the same compose
/// stack -- or may briefly go away, and a worker waits for it rather than
/// exiting. Except when it was revoked: that is returned, so the process exits
/// loudly for the operator instead of retrying forever. A server that speaks
/// no protocol this worker does is waited for too, but slowly, and said once.
pub async fn enroll(cfg: &CoreConfig, identity: &Identity, kind: &str) -> Result<WorkerClient> {
    let mut refused: Option<String> = None;
    loop {
        match ensure_enrolled(cfg, identity, kind).await {
            Ok(client) => return Ok(client),
            Err(e) if e.downcast_ref::<Revoked>().is_some() => return Err(e),
            Err(e) if e.downcast_ref::<Incompatible>().is_some() => {
                let message = format!("{e:#}");
                if refused.as_ref() != Some(&message) {
                    tracing::error!("{message}; trying again every {}s", REFUSED_RETRY.as_secs());
                    refused = Some(message);
                }
                tokio::time::sleep(REFUSED_RETRY).await;
            }
            Err(e) => {
                tracing::warn!(
                    "Enrollment not complete ({e:#}); retrying in {}s",
                    cfg.poll_interval
                );
                tokio::time::sleep(Duration::from_secs(cfg.poll_interval)).await;
            }
        }
    }
}

/// Ensure the worker is enrolled and return an authenticated mTLS client.
///
/// The worker registers on **every** startup, not only the first. Registration
/// carries its name, arches, concurrency, priority and package affinity — all of
/// which come from the environment and any of which may have been edited since
/// the last boot — so skipping it would let the server's picture of the fleet
/// drift from reality. Registration is idempotent: it is keyed on the stable
/// SPKI fingerprint of the persisted keypair, and it never alters approval
/// state, so a revoked worker that comes back stays revoked (it does refresh
/// `last_seen`, which is how the UI can show that the machine is asking again).
///
/// It is also where this worker and the server agree on a worker protocol, so
/// it has to succeed before any work, even with a signed certificate already
/// persisted: a worker upgraded while the server was down would otherwise go
/// on speaking whatever the server agreed with its previous binary. A failure
/// is returned, and [`enroll`] tries again. That costs nothing -- a worker that
/// cannot reach the server to register could not claim a job from it either --
/// and the bundled topology, which starts the worker and the server from one
/// `docker compose up`, sees a few retries instead of an unregistered start.
pub async fn ensure_enrolled(
    cfg: &CoreConfig,
    identity: &Identity,
    kind: &str,
) -> Result<WorkerClient> {
    tracing::info!("Worker fingerprint: {}", identity.fingerprint);

    let csr_pem = identity.generate_csr(&cfg.name)?;

    if identity.is_enrolled() {
        // Reuse the CA we already pinned rather than re-running trust-on-first-use.
        let ca_pem = identity.ca_pem()?;
        let request = register_request(cfg, csr_pem.clone(), kind);
        let client = WorkerClient::enrollment(&cfg.aurcache_url, &ca_pem)?;
        let status = match client.register(&request).await {
            Ok(status) => status,
            Err(e) if e.downcast_ref::<Incompatible>().is_some() => return Err(e),
            Err(e) => {
                // A rotated CA looks exactly like this: the pinned CA cannot
                // verify the server's new certificate, so the connection fails
                // before anything else can run.
                if let Some(client) =
                    recover_from_rotated_ca(cfg, identity, &csr_pem, kind, &e).await?
                {
                    return Ok(client);
                }
                return Err(e.context("registering"));
            }
        };
        tracing::info!(
            "Registered (status: {}, worker protocol {})",
            status.status,
            status
                .protocol
                .map_or_else(|| "unknown".to_string(), |p| p.to_string())
        );
        // Revocation is terminal, like on the fresh path below: a revoked
        // worker that carried on would run forever failing every claim,
        // instead of exiting loudly for the operator.
        if status.status == ApprovalStatus::Revoked {
            return Err(Revoked.into());
        }
        // Adopt a certificate the server has re-issued instead of discarding
        // it. The server re-issues when the one it had on file was signed by a
        // CA it no longer has, and a worker that ignored the replacement kept
        // presenting a certificate nothing could verify.
        if let Some(client) = try_finish_enrollment(cfg, identity, &status, &request)? {
            return Ok(client);
        }

        tracing::info!("Reusing persisted worker certificate");
        let client = WorkerClient::authenticated(
            &cfg.aurcache_url,
            &ca_pem,
            &identity.cert_pem()?,
            &identity.key_pem(),
        )?;
        client.remember_registration(request);
        return Ok(client);
    }

    // 1. Pin the server CA (verified against a configured fingerprint or TOFU).
    let ca_pem = fetch_and_pin_ca(&cfg.aurcache_url, cfg.server_ca_fingerprint.as_deref())
        .await
        .context("pinning server CA")?;

    let enroll_client = WorkerClient::enrollment(&cfg.aurcache_url, &ca_pem)?;

    // 2. Submit our CSR and publish it for co-located auto-approval.
    publish_csr_to_enrollment_dir(cfg, &identity.fingerprint, &csr_pem);

    let request = register_request(cfg, csr_pem, kind);
    let mut status = enroll_client
        .register(&request)
        .await
        .context("registering")?;

    // 3. Poll until approved.
    loop {
        if let Some(client) = try_finish_enrollment(cfg, identity, &status, &request)? {
            tracing::info!("Worker approved and enrolled");
            return Ok(client);
        }
        if status.status == ApprovalStatus::Revoked {
            return Err(Revoked.into());
        }
        tracing::info!("Awaiting approval (status: {})…", status.status);
        tokio::time::sleep(Duration::from_secs(cfg.poll_interval)).await;
        status = enroll_client
            .register_status(&identity.fingerprint)
            .await
            .context("polling enrollment status")?;
    }
}

/// Re-pin the server's CA and enrol again, when the pinned one has stopped
/// working because the server's CA was regenerated.
///
/// Only attempted when `AURCACHE_SERVER_CA_FINGERPRINT` is configured, and the
/// newly fetched CA is checked against it. That is the whole reason this is
/// safe to do automatically: re-pinning on a failed connection is otherwise
/// indistinguishable from accepting whatever an attacker offers, which is
/// precisely what pinning exists to prevent.
///
/// Without a configured fingerprint the situation is reported rather than
/// papered over, because only the operator can tell a legitimate rotation from
/// an interception.
///
/// Returns `None` when recovery was not attempted or did not apply, leaving the
/// caller to register again later -- a server that is merely not listening yet
/// must not cost a worker its enrolment.
async fn recover_from_rotated_ca(
    cfg: &CoreConfig,
    identity: &Identity,
    csr_pem: &str,
    kind: &str,
    cause: &anyhow::Error,
) -> Result<Option<WorkerClient>> {
    let Some(expected) = cfg.server_ca_fingerprint.as_deref() else {
        // Once per process: registering is retried every poll interval, and
        // most failures are a server that is merely down, which this advice
        // would otherwise follow every time.
        static ADVISED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !ADVISED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                "Registering failed ({cause:#}). If the server's CA was regenerated \
                 this worker cannot tell that apart from an interception, so it will keep \
                 trying with the CA it pinned. Set AURCACHE_SERVER_CA_FINGERPRINT to let it \
                 recover on its own, or remove the pinned CA to enrol afresh."
            );
        }
        return Ok(None);
    };

    let Ok(fresh_ca) = fetch_and_pin_ca(&cfg.aurcache_url, Some(expected)).await else {
        // Unreachable, or offering a CA that fails the fingerprint check. Either
        // way this is not a rotation we can act on.
        return Ok(None);
    };

    if identity.ca_pem().is_ok_and(|pinned| pinned == fresh_ca) {
        // Same CA as before, so the failure was something else -- the server
        // starting up, most likely.
        return Ok(None);
    }

    tracing::warn!(
        "The server's CA has changed and matches the configured fingerprint; \
         re-enrolling with the new one"
    );
    let client = WorkerClient::enrollment(&cfg.aurcache_url, &fresh_ca)?;
    publish_csr_to_enrollment_dir(cfg, &identity.fingerprint, csr_pem);
    let request = register_request(cfg, csr_pem.to_string(), kind);
    let status = client
        .register(&request)
        .await
        .context("re-registering after the server CA changed")?;
    try_finish_enrollment(cfg, identity, &status, &request)
}

/// If the status carries a signed certificate, persist it and build the
/// authenticated client, which sends `request` again whenever the server asks
/// this worker to register. Returns `None` while still pending.
fn try_finish_enrollment(
    cfg: &CoreConfig,
    identity: &Identity,
    status: &RegisterStatus,
    request: &RegisterRequest,
) -> Result<Option<WorkerClient>> {
    let (Some(cert), Some(ca)) = (&status.signed_cert, &status.ca_cert) else {
        return Ok(None);
    };
    identity.store_signed(cert, ca)?;
    let mut client = WorkerClient::authenticated(&cfg.aurcache_url, ca, cert, &identity.key_pem())?;
    client.remember_registration(request.clone());

    // Render the server's `[repo]` template with the host we actually reach it
    // on. Done here, once, because it describes the deployment rather than any
    // one build.
    let section = status
        .repo_template
        .as_deref()
        .map(|template| {
            crate::repo::render(
                template,
                &cfg.aurcache_url,
                cfg.repo_host.as_deref(),
                cfg.repo_url.as_deref(),
            )
        })
        .unwrap_or_default();
    match section.lines().find(|l| l.starts_with("Server =")) {
        Some(server) => tracing::info!("Package repository for this worker: {server}"),
        None => tracing::warn!(
            "No usable [repo] section for this worker; builds cannot resolve \
             AURCache-built dependencies"
        ),
    }
    client.set_repo_section(section);
    Ok(Some(client))
}
