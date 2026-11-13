//! Remote-worker HTTP protocol (`/api/worker/*`) plus admin management
//! endpoints (`/api/workers`).
//!
//! Workers authenticate with mutual TLS: their client certificate is signed by
//! AURCache's internal CA and mapped to a `workers` row by the SHA-256
//! fingerprint of its public key. Enrollment endpoints (`register`, `ca`,
//! `register status`) are reachable without a client certificate so a worker
//! can bootstrap; all job endpoints require an *approved* worker's certificate.
//!
//! The rules live in `aurcache-utils` (`worker_enroll`, `job_descriptor`,
//! `worker_heartbeat`, `worker_settings`, `worker_fleet`, `worker_complete`);
//! each handler here checks the caller, calls one, and maps the outcome.

use crate::auth::Authenticated;
use crate::utils::error::{ApiError, err};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_ca::Ca;
use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::{BuildRef, WorkerRef};
use aurcache_common::api::worker::{
    ApprovalStatus, WorkerConfigUpdate, WorkerConfigView, WorkerJoinInfo, WorkerSummary,
};
use aurcache_common::worker::{
    ClaimRequest, CompleteReport, Heartbeat, HeartbeatResponse, JobDescriptor, RegisterRequest,
    RegisterStatus, WorkerLogReport,
};
use aurcache_db::helpers::{worker_jobs, worker_store};
use aurcache_db::workers;
use aurcache_utils::build_logger::append_build_output;
use aurcache_utils::repository::Repository;
use aurcache_utils::services::Services;
use aurcache_utils::settings;
use aurcache_utils::snapshot::SnapshotStore;
use aurcache_utils::worker_complete;
use aurcache_utils::worker_enroll::{EnrollmentPolicy, Registered};
use aurcache_utils::worker_policy::worker_policy;
use aurcache_utils::worker_settings::Save;
use rocket::data::ToByteUnit;
use rocket::http::Status;
use rocket::mtls::Certificate;
use rocket::request::{FromRequest, Outcome, Request};
use rocket::serde::json::Json;
use rocket::{Data, State, get, patch, post};
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use utoipa::OpenApi;

/// A `[repo]` section for this instance with the host left as a placeholder,
/// sent with every registration status.
///
/// Scheme, port and path come from `AURCACHE_PUBLIC_URL` because those describe
/// how the repository is *published*; only the host is replaced, because that
/// is the one part each worker knows better than the server does. A worker in
/// the compose network, one on the LAN and one embedded in this very container
/// all reach the same repository by different names.
static REPO_TEMPLATE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| render_repo_template(&public_repo_url()));

/// Base URL workers should use for the public pacman repo (`[repo] Server`).
pub(crate) fn public_repo_url() -> String {
    aurcache_common::server_env::AURCACHE_PUBLIC_URL
        .get()
        .unwrap_or_else(|_| {
            format!(
                "http://localhost:{}",
                aurcache_common::ports::AURCACHE_MIRROR_PORT
            )
        })
}

/// [`REPO_TEMPLATE`] for `public_url`.
///
/// The host substitution is shared with the CLI, which answers the same
/// question for a `pacman.conf` — see [`aurcache_common::repo`].
fn render_repo_template(public_url: &str) -> String {
    let server = aurcache_common::repo::server_url_for_host(
        public_url,
        aurcache_common::worker::REPO_HOST_PLACEHOLDER,
    );
    let name = aurcache_common::repo::REPO_NAME;
    format!("[{name}]\nSigLevel = Never\nServer = {server}\n")
}

/// The worker protocol each worker agreed on at registration, by certificate
/// fingerprint.
///
/// Memory only. An agreement means something to the server process that made
/// it, so a restart -- an upgrade above all -- starts empty, and each worker
/// registers again the first time [`WorkerAuth`] turns it away.
#[derive(Default)]
pub struct AgreedProtocols(std::sync::Mutex<HashMap<String, u32>>);

impl AgreedProtocols {
    fn agree(&self, fingerprint: &str, protocol: u32) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(fingerprint.to_string(), protocol);
    }

    fn of(&self, fingerprint: &str) -> Option<u32> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(fingerprint)
            .copied()
    }
}

/// Request guard: a verified, CA-signed client certificate mapped to an
/// **approved** worker row. Pending/revoked/unknown workers are refused.
///
/// So is an approved worker this server process holds no protocol agreement
/// for, with `428 Precondition Required`: it registers again and retries. That
/// is how every worker renegotiates after the server restarts, so none of them
/// goes on speaking a protocol a new server no longer does.
pub struct WorkerAuth {
    pub worker: workers::Model,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for WorkerAuth {
    type Error = String;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let cert = match req.guard::<Certificate<'r>>().await {
            Outcome::Success(c) => c,
            Outcome::Forward(s) => return Outcome::Forward(s),
            Outcome::Error((s, _)) => {
                return Outcome::Error((s, "invalid client certificate".to_string()));
            }
        };
        // `Certificate` derefs to the TBS certificate; hash its SubjectPublicKeyInfo.
        let fingerprint = aurcache_ca::fingerprint_from_spki_der(cert.subject_pki.raw);

        let Some(db) = req.rocket().state::<DatabaseConnection>() else {
            return Outcome::Error((Status::InternalServerError, "no db".to_string()));
        };
        match worker_store::find_worker_by_fingerprint(db, &fingerprint).await {
            Ok(Some(mut worker)) if worker.status == ApprovalStatus::Approved => {
                let agreed = req
                    .rocket()
                    .state::<AgreedProtocols>()
                    .and_then(|protocols| protocols.of(&fingerprint));
                if agreed.is_none() {
                    return Outcome::Error((
                        Status::PreconditionRequired,
                        "register first: this server holds no protocol agreed with this worker"
                            .to_string(),
                    ));
                }
                let _ = worker_store::touch_last_seen(db, &mut worker, None).await;
                Outcome::Success(Self { worker })
            }
            Ok(_) => Outcome::Error((Status::Forbidden, "worker not approved".to_string())),
            Err(e) => Outcome::Error((Status::InternalServerError, e.to_string())),
        }
    }
}

#[derive(OpenApi)]
#[openapi(paths(
    register_worker,
    register_status,
    get_ca,
    get_ca_fingerprint,
    claim_job,
    job_source,
    job_logs,
    worker_log,
    job_artifact,
    complete_job,
    heartbeat,
    list_workers,
    worker_config,
    update_worker_config,
    worker_join_info,
    approve_worker,
    pause_worker,
    resume_worker,
    revoke_worker
))]
pub struct WorkerApi;

/// Remote-worker **protocol** routes (mounted under `/api` on the dedicated
/// mTLS worker listener). Enrollment (`register`, `ca`, status) is reachable
/// without a client certificate; all job endpoints require an approved
/// worker's certificate.
#[must_use]
pub fn worker_protocol_routes() -> Vec<rocket::Route> {
    rocket::routes![
        register_worker,
        register_status,
        get_ca,
        get_ca_fingerprint,
        claim_job,
        job_source,
        job_logs,
        worker_log,
        job_artifact,
        complete_job,
        heartbeat,
    ]
}

/// Worker **admin** routes (mounted under `/api` on the main HTTP API listener).
/// These use operator/session authentication, not mTLS, so they live on the
/// human-facing plane alongside the rest of the REST API and the web UI.
#[must_use]
pub fn worker_admin_routes() -> Vec<rocket::Route> {
    rocket::routes![
        list_workers,
        worker_config,
        update_worker_config,
        worker_join_info,
        approve_worker,
        pause_worker,
        resume_worker,
        revoke_worker,
    ]
}

/// Default worker image, published alongside every release. Overridden per
/// deployment with `AURCACHE_WORKER_IMAGE` (a private registry, a pinned tag).
const DEFAULT_WORKER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-worker:latest";

// ----------------------------------------------------------------------------
// Enrollment (no client certificate required)
// ----------------------------------------------------------------------------

/// The refusals [`WorkerAuth`] answers with, for the documentation of every
/// route it guards. Never built: the guard answers with the status alone.
#[derive(utoipa::IntoResponses)]
#[allow(dead_code)]
pub enum WorkerAuthRefusal {
    /// Not an approved worker, or not the worker that holds this build
    #[response(status = 403)]
    NotApproved,
    /// This server holds no protocol agreed with the worker: register again,
    /// then retry
    #[response(status = 428)]
    RegisterAgain,
}

/// Register (or re-register) a worker. Idempotent by CSR fingerprint. The CSR is
/// signed immediately so the certificate is ready the moment an admin approves;
/// the worker only receives it (and may only use it) once approved.
///
/// This is also where the worker protocol is agreed: the newest one both sides
/// speak. A worker that shares none is refused with `426 Upgrade Required`
/// before anything about it is recorded, so it never reaches the approval
/// list.
#[utoipa::path(post, path = "/worker/register", responses(
    (status = 200, body = RegisterStatus),
    (status = 400, description = "Not a CSR this server can read"),
    (status = 426, description = "No worker protocol in common; the body says which each side speaks"),
))]
#[post("/worker/register", data = "<input>")]
pub async fn register_worker(
    db: &State<DatabaseConnection>,
    ca: &State<Ca>,
    al: &State<ActivityLog>,
    policy: &State<EnrollmentPolicy>,
    protocols: &State<AgreedProtocols>,
    input: Json<RegisterRequest>,
) -> Result<Json<RegisterStatus>, ApiError> {
    let db = db.inner();
    match aurcache_utils::worker_enroll::register(db, ca, al, policy, &input).await? {
        Registered::Agreed {
            fingerprint,
            protocol,
        } => {
            protocols.agree(&fingerprint, protocol);
            register_status_for(db, ca, &fingerprint, Some(protocol)).await
        }
        Registered::InvalidCsr(why) => Err(err(Status::BadRequest, why)),
        Registered::NoCommonProtocol(why) => Err(err(Status::UpgradeRequired, why)),
    }
}

/// Poll enrollment status by fingerprint (the worker derives its own).
#[utoipa::path(get, path = "/worker/register/{fingerprint}/status", responses((status = 200, body = RegisterStatus)))]
#[get("/worker/register/<fingerprint>/status")]
pub async fn register_status(
    db: &State<DatabaseConnection>,
    ca: &State<Ca>,
    protocols: &State<AgreedProtocols>,
    fingerprint: &str,
) -> Result<Json<RegisterStatus>, ApiError> {
    let protocol = protocols.of(fingerprint);
    register_status_for(db.inner(), ca, fingerprint, protocol).await
}

async fn register_status_for(
    db: &DatabaseConnection,
    ca: &Ca,
    fingerprint: &str,
    protocol: Option<u32>,
) -> Result<Json<RegisterStatus>, ApiError> {
    let worker = worker_store::find_worker_by_fingerprint(db, fingerprint)
        .await?
        .ok_or_else(|| err(Status::NotFound, "worker not registered"))?;

    // Only release the certificate + CA once the worker is approved.
    let (signed_cert, ca_cert) = if worker.status == ApprovalStatus::Approved {
        (
            worker.signed_cert.clone(),
            Some(ca.ca_cert_pem().to_string()),
        )
    } else {
        (None, None)
    };

    Ok(Json(RegisterStatus {
        status: worker.status,
        signed_cert,
        ca_cert,
        repo_template: Some(REPO_TEMPLATE.clone()),
        agreement: protocol.map(|protocol| worker_policy().agreement(protocol)),
    }))
}

/// Fetch the CA certificate so a worker can pin the server's identity.
#[utoipa::path(get, path = "/worker/ca", responses((status = 200, description = "PEM CA certificate")))]
#[get("/worker/ca")]
pub fn get_ca(ca: &State<Ca>) -> String {
    ca.ca_cert_pem().to_string()
}

/// Return the CA certificate's SHA-256 fingerprint so a worker can pin the
/// server before trusting any issued certificate (anti-MITM during enrollment).
#[utoipa::path(get, path = "/worker/ca/fingerprint", responses((status = 200, description = "CA fingerprint")))]
#[get("/worker/ca/fingerprint")]
pub fn get_ca_fingerprint(ca: &State<Ca>) -> Result<String, ApiError> {
    Ok(ca.ca_cert_fingerprint()?)
}

// ----------------------------------------------------------------------------
// Job lifecycle (approved worker certificate required)
// ----------------------------------------------------------------------------

/// What a claim gets: a job, or nothing to do.
// Built once per request and handed straight to Rocket; boxing the job to
// shrink it would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(rocket::Responder)]
pub enum Claim {
    Job(Json<JobDescriptor>),
    /// `204 No Content`: the queue has nothing for this worker. An idle
    /// worker asks every few seconds, so this is the common answer and must
    /// not read as an error.
    #[response(status = 204)]
    Idle(()),
}

/// Claim the next buildable job for this worker, or 204 if none.
///
/// Routing reads arches, package affinity and priority from the worker's
/// stored row, not from the claim: each worker's decision depends on what
/// every *other* worker declared, so those values must come from one
/// consistent source. The row is refreshed on every re-registration, which
/// happens on each worker boot.
#[utoipa::path(post, path = "/worker/jobs/claim", request_body = ClaimRequest, responses(
    (status = 200, description = "A job; its lease starts now and is renewed by heartbeats", body = JobDescriptor),
    (status = 204, description = "Nothing for this worker; ask again later"),
        WorkerAuthRefusal,
))]
#[post("/worker/jobs/claim", data = "<claim>")]
pub async fn claim_job(
    db: &State<DatabaseConnection>,
    store: &State<Arc<SnapshotStore>>,
    al: &State<ActivityLog>,
    auth: WorkerAuth,
    claim: Json<ClaimRequest>,
) -> Result<Claim, ApiError> {
    let db = db.inner();

    let Some(build) = worker_jobs::claim_job(
        db,
        auth.worker.id,
        worker_policy().lease_ttl_secs,
        worker_policy().spill_delay_secs,
        worker_policy().liveness_timeout_secs,
    )
    .await?
    else {
        return Ok(Claim::Idle(()));
    };

    let descriptor =
        aurcache_utils::job_descriptor::descriptor(db, store, &build, &claim.mirrorlist).await?;
    al.emit(Event::BuildStarted {
        build: BuildRef {
            pkgbase: descriptor.pkgbase.clone(),
            number: build.number,
        },
        worker: auth.worker.name.as_str().into(),
    });
    Ok(Claim::Job(Json(descriptor)))
}

/// Stream the (server-patched) source archive for a claimed job.
#[utoipa::path(get, path = "/worker/jobs/{build_id}/source", responses(
    (status = 200, description = "The package's sources as a gzipped tarball, with the server's patches applied", content_type = "application/gzip"),
    (status = 404, description = "No such build"),
        WorkerAuthRefusal,
))]
#[get("/worker/jobs/<build_id>/source")]
pub async fn job_source(
    db: &State<DatabaseConnection>,
    store: &State<Arc<SnapshotStore>>,
    auth: WorkerAuth,
    build_id: i32,
) -> Result<Vec<u8>, ApiError> {
    let db = db.inner();
    let build = worker_complete::assert_owned_active(db, auth.worker.id, build_id)
        .await
        .map_err(completion_error)?;
    let pkg = crate::package::package_by_id(db, build.pkg_id).await?;
    Ok(store
        .archive_bytes(&pkg.source_data, pkg.patch.as_deref())
        .await?)
}

/// Append build log output.
#[utoipa::path(post, path = "/worker/jobs/{build_id}/logs",
    request_body(content = String, description = "Build output to append, as UTF-8 text", content_type = "text/plain"),
    responses(
        (status = 200, description = "Appended"),
        (status = 400, description = "Not text, or the build's log is full"),
        WorkerAuthRefusal,
    ),
)]
#[post("/worker/jobs/<build_id>/logs", data = "<data>")]
pub async fn job_logs(
    db: &State<DatabaseConnection>,
    auth: WorkerAuth,
    build_id: i32,
    data: Data<'_>,
) -> Result<(), ApiError> {
    let db = db.inner();
    let build = worker_complete::assert_owned_active(db, auth.worker.id, build_id)
        .await
        .map_err(completion_error)?;

    let text = data
        .open(4.mebibytes())
        .into_string()
        .await
        .map_err(|e| err(Status::BadRequest, e))?;

    // Logs are stored under the build's public identity, and the worker
    // endpoints are the only ones keyed by the internal id. `build` comes from
    // the ownership check above, so this costs one lookup for the pkgbase.
    let pkgbase = crate::package::package_by_id(db, build.pkg_id).await?.name;

    // The worker posts whole chunks, so this needs no buffering task of its own.
    append_build_output(&pkgbase, build.number, &text).await?;
    Ok(())
}

/// A problem a worker noticed on its own account -- not a protocol failure
/// the server already tracks (a lost claim, a missed heartbeat: the server
/// finds those out for itself, from the same silence that would swallow a
/// report about them), but something local: a build that needed a fallback to
/// finish, or maintenance that could not complete cleanly.
///
/// `Severity::Info` is not expected on this channel and is filed as a warning
/// rather than refused, so an older or misconfigured worker cannot fail a
/// build over what is, at worst, a noisy log line.
#[utoipa::path(post, path = "/worker/log", request_body = WorkerLogReport, responses(
    (status = 200, description = "Recorded in the activity log, under this worker"),
        WorkerAuthRefusal,
))]
#[post("/worker/log", data = "<input>")]
pub async fn worker_log(
    db: &State<DatabaseConnection>,
    al: &State<ActivityLog>,
    auth: WorkerAuth,
    input: Json<WorkerLogReport>,
) -> Result<(), ApiError> {
    let db = db.inner();
    let input = input.into_inner();
    let worker = WorkerRef::from(auth.worker.name.as_str());

    // Ownership only, not `assert_owned_active`: the report is a comment on
    // the build, not a claim on its state, and one can arrive just after the
    // build's terminal state lands (cleanup, an upload still finishing).
    let build = match input.build_id {
        Some(build_id) => {
            let build = worker_complete::assert_owned(db, auth.worker.id, build_id)
                .await
                .map_err(completion_error)?;
            let pkgbase = crate::package::package_by_id(db, build.pkg_id).await?.name;
            Some(BuildRef {
                pkgbase,
                number: build.number,
            })
        }
        None => None,
    };

    al.emit(match input.severity {
        Severity::Error => Event::WorkerError {
            worker,
            build,
            message: input.message,
        },
        Severity::Warning | Severity::Info => Event::WorkerWarning {
            worker,
            build,
            message: input.message,
        },
    });
    Ok(())
}

/// The size a request says its body is, from `Content-Length`, when it says.
///
/// What lets an artifact over the limit be refused before it is read. Without
/// it the only way to find out is to read up to the limit: a 40 GiB package
/// arrived as 20 GiB copied to the server's disk, then deleted, then refused.
pub struct DeclaredLength(Option<u64>);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for DeclaredLength {
    type Error = std::convert::Infallible;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        Outcome::Success(Self(
            req.headers()
                .get_one("Content-Length")
                .and_then(|v| v.trim().parse().ok()),
        ))
    }
}

/// Stream one built artifact into the job's staging area.
///
/// The body is copied to disk in chunks rather than buffered in memory: a
/// package file can be multi-gigabyte, and `into_bytes` would hold it all in
/// RAM, stalling the upload until it timed out under memory pressure.
#[utoipa::path(post, path = "/worker/jobs/{build_id}/artifacts/{filename}",
    request_body(content = Vec<u8>, description = "One built package file, streamed; send Content-Length when known so an oversized one is refused before any of it is sent", content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Staged; published with the rest when the build completes"),
        (status = 400, description = "Not an acceptable artifact name"),
        (status = 413, description = "Larger than this build may produce; the body says which limit"),
        WorkerAuthRefusal,
    ),
)]
#[post("/worker/jobs/<build_id>/artifacts/<filename>", data = "<data>")]
pub async fn job_artifact(
    db: &State<DatabaseConnection>,
    repo: &State<Arc<Repository>>,
    auth: WorkerAuth,
    build_id: i32,
    filename: &str,
    declared: DeclaredLength,
    data: Data<'_>,
) -> Result<(), ApiError> {
    let db = db.inner();
    let build = worker_complete::assert_owned_active(db, auth.worker.id, build_id)
        .await
        .map_err(completion_error)?;

    // A backstop against a runaway or hostile PKGBUILD filling the server's
    // disk, not a statement about what a package should weigh -- which is why
    // it is a setting, and per package: `unreal-engine` is past 20 GiB, and
    // allowing it that should not allow everything else the same.
    let limit = settings::get(db, settings::key::MAX_ARTIFACT_SIZE, Some(build.pkg_id)).await?;
    let (limit, source) = (limit.value.0, limit.source);
    let too_large = || {
        err(
            Status::PayloadTooLarge,
            format!(
                "{filename} is larger than this package's artifact limit of {} \
                 (max_artifact_size, from {source:?}); raise it for the package or globally",
                aurcache_common::units::format_size(limit)
            ),
        )
    };
    if declared.0.is_some_and(|len| len > limit) {
        return Err(too_large());
    }

    // Reject path traversal; only a bare filename is allowed.
    let name = Path::new(filename)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| *s == filename)
        .ok_or_else(|| err(Status::BadRequest, "invalid filename"))?;

    let dir = repo.staging_dir(build_id);
    tokio::fs::create_dir_all(&dir).await?;

    let dest = dir.join(name);
    // `open` stops reading at the limit without saying so; `is_complete` is the
    // only thing that tells a whole artifact from one cut off there. `into_file`
    // also flushes, so the file is fully written by the time publishing reads
    // it.
    let stored = data.open(limit.bytes()).into_file(&dest).await;
    let problem = match &stored {
        // The request parsed: a write failure is the server's disk, not the
        // worker's artifact, so it is a 500. Size and emptiness rejections
        // below stay 400s — those describe the upload.
        Err(e) => Some(err(Status::InternalServerError, e)),
        Ok(file) if !file.is_complete() => Some(too_large()),
        Ok(file) if file.n.written == 0 => Some(err(Status::BadRequest, "empty artifact")),
        Ok(_) => None,
    };
    if let Some(problem) = problem {
        // Leave no partial file behind for publishing to trip over.
        drop(stored);
        let _ = tokio::fs::remove_file(&dest).await;
        return Err(problem);
    }
    Ok(())
}

/// Report a build as complete.
///
/// A success is accepted and answered at once: the artifacts are all uploaded,
/// so the worker's part is done, and the build moves to `PUBLISHING` while the
/// server puts it in the repository in the background (see
/// [`publish_with_recovery`]). Nothing that can go wrong from there is the worker's to
/// fix. A failure is terminal. An interruption is abandoned like a build whose
/// worker went silent, and queued again while attempts remain.
#[utoipa::path(post, path = "/worker/jobs/{build_id}/complete", request_body = CompleteReport, responses(
    (status = 200, description = "Accepted, or a repeat of a report already accepted; either way the worker is done with the build"),
    (status = 404, description = "No such build"),
        WorkerAuthRefusal,
))]
#[post("/worker/jobs/<build_id>/complete", data = "<input>")]
pub async fn complete_job(
    db: &State<DatabaseConnection>,
    repo: &State<Arc<Repository>>,
    al: &State<ActivityLog>,
    auth: WorkerAuth,
    build_id: i32,
    input: Json<CompleteReport>,
) -> Result<(), ApiError> {
    // A refused completion is how a build that finished becomes a build that
    // failed, so the reason is logged here as well as answered: the worker's
    // journal is on another machine, and the activity log is where an
    // operator looks. Refusing quietly is what made a stale `files` row take
    // an afternoon to find.
    let outcome = worker_complete::complete(
        db.inner(),
        repo.inner(),
        al.inner(),
        &auth.worker,
        build_id,
        input.into_inner(),
    )
    .await
    .map(|_| ())
    .map_err(completion_error);
    if let Err(e) = &outcome {
        tracing::warn!(
            "rejected completion of build {build_id} from worker {}: {} -- {}",
            auth.worker.id,
            e.status,
            e.message
        );
        // A build nobody can name any more -- its package deleted -- leaves
        // only the journal line above.
        if let Ok(named) = aurcache_db::helpers::builds::build_refs(db.inner(), &[build_id]).await
            && let Some(build) = named.into_iter().next()
        {
            al.emit(Event::BuildCompletionRejected {
                build,
                worker: auth.worker.name.as_str().into(),
                reason: e.message.clone(),
            });
        }
    }
    outcome
}

fn completion_error(error: worker_complete::CompletionError) -> ApiError {
    use worker_complete::CompletionError;
    let status = match &error {
        CompletionError::NotFound(_) => Status::NotFound,
        CompletionError::LeaseLost { .. } => Status::Forbidden,
        CompletionError::Persistence(_) => Status::InternalServerError,
    };
    err(status, error)
}

/// Liveness heartbeat: renews leases for reported builds, abandons any this
/// worker silently dropped, and returns the builds the server wants it to stop
/// (abandoned, or cancelled by an operator). The abort list rides this same
/// answer rather than a second poll, so a cancelled or lost build is stopped on
/// the worker's next heartbeat.
#[utoipa::path(post, path = "/worker/heartbeat", request_body = Heartbeat, responses(
    (status = 200, description = "Leases renewed for the builds listed; the answer names builds to stop and carries new settings", body = HeartbeatResponse),
        WorkerAuthRefusal,
))]
#[post("/worker/heartbeat", data = "<input>")]
pub async fn heartbeat(
    db: &State<DatabaseConnection>,
    mut auth: WorkerAuth,
    al: &State<ActivityLog>,
    input: Json<Heartbeat>,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    let response =
        aurcache_utils::worker_heartbeat::heartbeat(db, al, &mut auth.worker, &input).await?;
    Ok(Json(response))
}

// ----------------------------------------------------------------------------
// Admin management (operator auth)
// ----------------------------------------------------------------------------

#[utoipa::path(get, path = "/workers", responses((status = 200, body = [WorkerSummary])))]
#[get("/workers")]
pub async fn list_workers(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<Vec<WorkerSummary>>, ApiError> {
    Ok(Json(aurcache_utils::worker_fleet::list(db).await?))
}

/// The image and port a copy-and-run `docker run` needs, for the Workers page
/// to show one when the fleet is empty. The host is not here on purpose — the
/// browser knows the address the page was reached on and the server does not.
#[utoipa::path(get, path = "/workers/join-info", responses((status = 200, body = WorkerJoinInfo)))]
#[get("/workers/join-info")]
pub async fn worker_join_info(_a: Authenticated) -> Json<WorkerJoinInfo> {
    let image = aurcache_common::server_env::AURCACHE_WORKER_IMAGE
        .get()
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_WORKER_IMAGE.to_string());
    Json(WorkerJoinInfo {
        image,
        worker_port: crate::init::worker_port(),
    })
}

/// What one worker can be configured with, and what it is running.
///
/// A stored blob that no longer parses is reported as absent rather than
/// failing the request; see [`aurcache_utils::worker_settings::view`].
#[utoipa::path(get, path = "/workers/{id}/config", responses((status = 200, body = WorkerConfigView)))]
#[get("/workers/<id>/config")]
pub async fn worker_config(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
    id: i32,
) -> Result<Json<WorkerConfigView>, ApiError> {
    let db = db.inner();
    let worker = find_or_404(db, id).await?;
    Ok(Json(
        aurcache_utils::worker_settings::view(db, &worker).await?,
    ))
}

/// Save values for several of a worker's settings at once.
///
/// Every value is checked against what the worker declared before anything is
/// written, and the whole set is written in one transaction: a save is either
/// taken whole or refused whole, never half-delivered. Removing a value needs
/// no check, so one the worker no longer declares can still be cleared.
///
/// The worker picks the change up on its next heartbeat. It may still refuse a
/// value this accepted -- a limit its host cannot enforce -- and says so in its
/// next report, which is what the page then shows.
#[utoipa::path(
    patch,
    path = "/workers/{id}/config",
    request_body = WorkerConfigUpdate,
    responses(
        (status = 200, body = WorkerConfigView),
        (status = 400, description = "A value the worker's declaration does not take; the body names each"),
    )
)]
#[patch("/workers/<id>/config", data = "<input>")]
pub async fn update_worker_config(
    services: &State<Services>,
    a: Authenticated,
    id: i32,
    input: Json<WorkerConfigUpdate>,
) -> Result<Json<WorkerConfigView>, ApiError> {
    let db = &services.db;
    let worker = find_or_404(db, id).await?;
    match aurcache_utils::worker_settings::save(db, &worker, input.into_inner().settings).await? {
        Save::Refused(why) => return Err(err(Status::BadRequest, why)),
        Save::Saved(saved) if saved.is_empty() => {}
        Save::Saved(saved) => services.activity.emit_by(
            Event::WorkerSettingsSaved {
                worker: worker.name.as_str().into(),
                set: saved.set,
                reset: saved.reset,
            },
            a.username,
        ),
    }
    Ok(Json(
        aurcache_utils::worker_settings::view(db, &worker).await?,
    ))
}

async fn find_or_404(db: &DatabaseConnection, id: i32) -> Result<workers::Model, ApiError> {
    worker_store::find_worker(db, id)
        .await?
        .ok_or_else(|| err(Status::NotFound, "no such worker"))
}

#[utoipa::path(post, path = "/workers/{id}/approve", responses((status = 200)))]
#[post("/workers/<id>/approve")]
pub async fn approve_worker(
    services: &State<Services>,
    a: Authenticated,
    id: i32,
) -> Result<(), ApiError> {
    let worker = worker_store::approve_worker(&services.db, id)
        .await?
        .ok_or_else(|| err(Status::NotFound, "no such worker"))?;
    services.activity.emit_by(
        Event::WorkerApproved {
            worker: worker.name.into(),
        },
        a.username,
    );
    Ok(())
}

/// Ask a worker to take no new builds and let the ones it holds finish.
///
/// For emptying a machine before a reboot, an upgrade or retirement without
/// cutting a build short. Takes effect at its next claim; it needs nothing
/// from the worker. Its builds keep running, it stays approved, and a
/// lower-priority worker stops waiting for it at once.
#[utoipa::path(post, path = "/workers/{id}/pause", responses((status = 200)))]
#[post("/workers/<id>/pause")]
pub async fn pause_worker(
    services: &State<Services>,
    a: Authenticated,
    id: i32,
) -> Result<(), ApiError> {
    set_paused(services, a, id, true).await
}

/// Let a paused worker take builds again.
#[utoipa::path(post, path = "/workers/{id}/resume", responses((status = 200)))]
#[post("/workers/<id>/resume")]
pub async fn resume_worker(
    services: &State<Services>,
    a: Authenticated,
    id: i32,
) -> Result<(), ApiError> {
    set_paused(services, a, id, false).await
}

async fn set_paused(
    services: &Services,
    a: Authenticated,
    id: i32,
    paused: bool,
) -> Result<(), ApiError> {
    let db = &services.db;
    let before = find_or_404(db, id).await?;
    // Only an approved worker claims anything to stop claiming. Pausing a
    // pending or revoked one would be a flag nobody could see take effect.
    if before.status != ApprovalStatus::Approved {
        return Err(err(
            Status::Conflict,
            format!("worker {} is {}, not approved", before.name, before.status),
        ));
    }
    let worker = worker_store::set_paused(db, id, paused)
        .await?
        .ok_or_else(|| err(Status::NotFound, "no such worker"))?;
    // Asking for what already holds changes nothing, and logs nothing.
    if before.paused != paused {
        let worker = worker.name.into();
        let event = if paused {
            Event::WorkerPaused { worker }
        } else {
            Event::WorkerResumed { worker }
        };
        services.activity.emit_by(event, a.username);
    }
    Ok(())
}

#[utoipa::path(post, path = "/workers/{id}/revoke", responses((status = 200)))]
#[post("/workers/<id>/revoke")]
pub async fn revoke_worker(
    services: &State<Services>,
    a: Authenticated,
    id: i32,
) -> Result<(), ApiError> {
    let db = &services.db;
    let revoked = worker_store::revoke_worker(db, id, worker_policy().max_attempts)
        .await?
        .ok_or_else(|| err(Status::NotFound, "no such worker"))?;
    services.activity.emit_by(
        Event::WorkerRevoked {
            worker: revoked.worker.name.into(),
            requeued: aurcache_utils::abandoned::named(&revoked.abandoned).retried,
        },
        a.username,
    );
    aurcache_utils::abandoned::report(db, &services.activity, &revoked.abandoned).await;
    Ok(())
}

#[cfg(test)]
mod repo_template_tests {
    use super::render_repo_template;
    use aurcache_common::worker::REPO_HOST_PLACEHOLDER;

    /// Only the host is replaced: the scheme, port and path describe how the
    /// repository is published and remain the server's decision.
    #[test]
    fn replaces_only_the_host() {
        let t = render_repo_template("http://aurcache.example.com:9000/repo");
        assert!(t.contains(&format!(
            "Server = http://{REPO_HOST_PLACEHOLDER}:9000/repo/$arch"
        )));
        assert!(t.starts_with("[repo]\nSigLevel = Never\n"));
    }

    /// A URL without an explicit port still needs one, or the worker would
    /// render `http://host/$arch` and reach the web UI instead of the repo.
    #[test]
    fn supplies_the_repository_port_when_the_url_omits_it() {
        let t = render_repo_template("http://aurcache.example.com");
        assert!(t.contains(&format!(
            "Server = http://{REPO_HOST_PLACEHOLDER}:{}/$arch",
            aurcache_common::ports::AURCACHE_MIRROR_PORT
        )));
    }

    #[test]
    fn keeps_https_and_ignores_a_trailing_slash() {
        let t = render_repo_template("https://aurcache.example.com:8081/");
        assert!(t.contains(&format!(
            "Server = https://{REPO_HOST_PLACEHOLDER}:8081/$arch"
        )));
    }
}

#[cfg(test)]
mod protocol_docs_tests {
    use super::{WorkerApi, worker_protocol_routes};
    use utoipa::OpenApi as _;

    /// Every route a worker calls is in the API documentation, which is the
    /// reference for a worker written somewhere else.
    #[test]
    fn every_protocol_route_is_documented() {
        let spec = WorkerApi::openapi();
        for route in worker_protocol_routes() {
            let path = route.uri.path().replace('<', "{").replace('>', "}");
            let item = spec.paths.paths.get(&path);
            let documented = item.is_some_and(|item| match route.method.as_str() {
                "GET" => item.get.is_some(),
                "POST" => item.post.is_some(),
                "PUT" => item.put.is_some(),
                "PATCH" => item.patch.is_some(),
                "DELETE" => item.delete.is_some(),
                _ => false,
            });
            assert!(documented, "{} {path} is not documented", route.method);
        }
    }
}
