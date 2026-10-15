//! Remote-worker HTTP protocol (`/api/worker/*`) plus admin management
//! endpoints (`/api/workers`).
//!
//! Workers authenticate with mutual TLS: their client certificate is signed by
//! AURCache's internal CA and mapped to a `workers` row by the SHA-256
//! fingerprint of its public key. Enrollment endpoints (`register`, `ca`,
//! `register status`) are reachable without a client certificate so a worker
//! can bootstrap; all job endpoints require an *approved* worker's certificate.

use crate::models::authenticated::Authenticated;
use crate::utils::error::{ApiError, err};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, WorkerReport};
use aurcache_ca::Ca;
use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::{BuildRef, WorkerRef};
use aurcache_common::api::worker::{
    ApprovalStatus, WorkerConfigUpdate, WorkerConfigView, WorkerJoinInfo, WorkerSummary,
};
use aurcache_common::build_state::{BuildState, EndReason};
use aurcache_common::worker::{
    BuildOutcome, ClaimRequest, CompleteReport, Heartbeat, HeartbeatResponse, JobDescriptor,
    JobStatus, MirrorlistOffer, MirrorlistPreference, PROTOCOLS, RegisterRequest, RegisterStatus,
    WorkerLogReport,
};
use aurcache_common::worker_config::{
    ConfigSnapshot, EffectiveConfig, SettingDecl, SettingStatus, validate_value,
};
use aurcache_db::helpers::time::now_secs;
use aurcache_db::helpers::{worker_jobs, worker_store};
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, workers};
use aurcache_utils::build_logger::append_build_output;
use aurcache_utils::job_config::{JobConfig, build_job_config, mirrorlist_dir, mirrorlist_for};
use aurcache_utils::publish::publish_with_recovery;
use aurcache_utils::repository::Repository;
use aurcache_utils::settings;
use aurcache_utils::snapshot::SnapshotStore;
use aurcache_utils::vcs_check::job_vcs_sources;
use aurcache_utils::worker_complete;
use aurcache_utils::worker_policy::worker_policy;
use rocket::data::ToByteUnit;
use rocket::http::Status;
use rocket::mtls::Certificate;
use rocket::request::{FromRequest, Outcome, Request};
use rocket::serde::json::Json;
use rocket::{Data, State, get, patch, post};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QuerySelect};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::path::Path;
use std::sync::Arc;
use utoipa::OpenApi;

/// How stale `last_seen` may be before a worker stops counting as available.
pub(crate) fn liveness_timeout_secs() -> i64 {
    worker_policy().liveness_timeout_secs
}

static REPO_TEMPLATE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| render_repo_template(&public_repo_url()));

/// Base URL workers should use for the public pacman repo (`[repo] Server`).
pub(crate) fn public_repo_url() -> String {
    env::var("AURCACHE_PUBLIC_URL").unwrap_or_else(|_| {
        format!(
            "http://localhost:{}",
            aurcache_common::ports::AURCACHE_MIRROR_PORT
        )
    })
}

/// A `[repo]` section for this instance with the host left as a placeholder.
///
/// Scheme, port and path come from `AURCACHE_PUBLIC_URL` because those describe
/// how the repository is *published*; only the host is replaced, because that
/// is the one part each worker knows better than the server does. A worker in
/// the compose network, one on the LAN and one embedded in this very container
/// all reach the same repository by different names.
fn repo_template() -> String {
    REPO_TEMPLATE.clone()
}

/// Pure form of [`repo_template`].
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

/// Checksum identifying one mirrorlist's content.
///
/// Computed only here. A worker never digests anything -- it echoes back the
/// value it was sent -- so this can change algorithm without any worker
/// needing to agree, at the cost of one resend per worker.
fn mirrorlist_checksum(content: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(content.as_bytes()))
}

/// The revision of a worker's server-set values, as its snapshot carries it.
///
/// A digest of the values themselves, keys in order (a `BTreeMap` serializes
/// that way), so it changes exactly when they do and needs no counter kept in
/// step across restarts. Like [`mirrorlist_checksum`], only ever computed here:
/// a worker echoes it back and never digests anything.
fn config_revision(values: &BTreeMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::to_string(values).unwrap_or_default();
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

/// Decide what to put in a job descriptor for the mirrorlist.
///
/// The mirrorlist is deployment
/// configuration rather than build configuration, but it is delivered on the
/// claim because it *changes* -- `MIRROR_RANK_SCHEDULE` rewrites it on a
/// schedule -- and the claim is the only exchange that recurs. Revalidating a
/// checksum keeps that from meaning the same bytes with every job.
fn resolve_mirrorlist(
    held: &MirrorlistPreference,
    arch: &str,
    current: Option<String>,
) -> MirrorlistOffer {
    // A worker with its own mirrorlist would discard anything sent, so nothing
    // is computed or sent for it.
    let MirrorlistPreference::Server { checksums } = held else {
        return MirrorlistOffer::Nothing;
    };
    let Some(content) = current else {
        return MirrorlistOffer::Nothing;
    };
    let checksum = mirrorlist_checksum(&content);
    if checksums.get(arch).map(String::as_str) == Some(checksum.as_str()) {
        MirrorlistOffer::Keep
    } else {
        MirrorlistOffer::Send { content, checksum }
    }
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
    job_status,
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
        job_status,
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
    (status = 426, description = "No worker protocol in common; the body says which each side speaks"),
))]
#[post("/worker/register", data = "<input>")]
pub async fn register_worker(
    db: &State<DatabaseConnection>,
    ca: &State<Ca>,
    al: &State<ActivityLog>,
    protocols: &State<AgreedProtocols>,
    input: Json<RegisterRequest>,
) -> Result<Json<RegisterStatus>, ApiError> {
    let db = db.inner();
    let input = input.into_inner();

    let fingerprint = aurcache_ca::fingerprint_from_csr_pem(&input.csr_pem)
        .map_err(|e| err(Status::BadRequest, e))?;

    let Some(protocol) = PROTOCOLS.negotiate(input.protocols) else {
        let reason = format!(
            "this server speaks worker protocol {PROTOCOLS}; worker '{}' speaks {}",
            input.name, input.protocols
        );
        tracing::warn!("Refused to register a worker: {reason}");
        return Err(err(Status::UpgradeRequired, reason));
    };

    // Asked before registering, because registering is an upsert and would
    // erase the difference: the first time is an enrollment, and every time
    // after is a worker coming back up.
    let known = worker_store::find_worker_by_fingerprint(db, &fingerprint)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .is_some();

    // Stored as JSON, because the server renders a declaration and never
    // reasons about it.
    let declaration = serde_json::to_string(&input.settings).unwrap_or_else(|e| {
        al.emit(Event::WorkerReportFailed {
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
            native_arches: &input.native_arches,
            emulated_arches: &input.emulated_arches,
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
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;

    if known {
        al.emit(Event::WorkerRegistered {
            worker: worker.name.clone().into(),
            version: Some(input.version.clone()).filter(|v| !v.is_empty()),
        });
    } else {
        al.emit(Event::WorkerEnrolled {
            worker: worker.name.clone().into(),
        });
    }

    // Issue the leaf certificate, or re-issue one this CA did not sign.
    //
    // The second case is a CA that has been regenerated -- a deployment that
    // lost the directory holding it, most often. Every certificate issued by
    // the old CA is then unverifiable, and a worker presenting one fails mTLS
    // with `BadSignature` on every request. Caching the certificate for ever
    // meant re-registration handed the same dead certificate back, so the
    // worker could never recover on its own however many times it restarted.
    let needs_cert = match worker.signed_cert.as_deref() {
        None => true,
        Some(cert) => !ca.issued(cert),
    };
    if needs_cert {
        let signed = ca
            .sign_worker_csr(&input.csr_pem, worker_policy().cert_validity_days)
            .map_err(|e| err(Status::InternalServerError, e))?;
        worker_store::store_signed_cert(db, worker.id, &signed.cert_pem, signed.not_after)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?
            // The row was read earlier in this request; gone now means a
            // concurrent delete raced registration, which is a server-side
            // inconsistency rather than a bad id.
            .ok_or_else(|| {
                err(
                    Status::InternalServerError,
                    "worker vanished mid-registration",
                )
            })?;
    }

    // Non-interactive enrollment: auto-approve when a configured mode matches.
    // Eligibility (pending only) is enforced inside `env_auto_approves`, so
    // a revoked worker is never re-approved by re-registering.
    if crate::worker_enroll::env_auto_approves(
        worker.status,
        &fingerprint,
        input.enrollment_token.as_deref(),
    ) {
        worker_store::approve_worker(db, worker.id)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?
            .ok_or_else(|| {
                err(
                    Status::InternalServerError,
                    "worker vanished mid-registration",
                )
            })?;
        tracing::info!("Auto-approved worker '{}' ({fingerprint})", worker.name);
        // No user: nobody clicked this. The log says the server did it.
        al.emit(Event::WorkerApproved {
            worker: worker.name.clone().into(),
        });
    }

    protocols.agree(&fingerprint, protocol);
    register_status_for(db, ca, &fingerprint, Some(protocol)).await
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
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
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
        repo_template: Some(repo_template()),
        protocol,
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
    ca.ca_cert_fingerprint()
        .map_err(|e| err(Status::InternalServerError, e))
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
    (status = 403, description = "Not an approved worker, or not the worker that holds this build"),
    (status = 428, description = "This server holds no protocol agreed with the worker: register again, then retry"),
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
        liveness_timeout_secs(),
    )
    .await
    .map_err(|e| err(Status::InternalServerError, e))?
    else {
        return Ok(Claim::Idle(()));
    };

    let descriptor = build_descriptor(db, store, &build, &claim.mirrorlist)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    al.emit(Event::BuildStarted {
        build: BuildRef {
            pkgbase: descriptor.pkgbase.clone(),
            number: build.number,
        },
        worker: auth.worker.name.as_str().into(),
    });
    Ok(Claim::Job(Json(descriptor)))
}

async fn build_descriptor(
    db: &DatabaseConnection,
    store: &SnapshotStore,
    build: &aurcache_db::builds::Model,
    held_mirrorlist: &MirrorlistPreference,
) -> anyhow::Result<JobDescriptor> {
    let pkg = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("package {} not found", build.pkg_id))?;

    let JobConfig {
        makepkg_conf,
        pacman_conf,
    } =
        // No `[repo]` section here: the worker appends one rendered from the
        // template it received at registration, using the host it actually
        // reaches this server on.
        build_job_config(db, pkg.id).await;

    let arch = build.platform.as_str().to_string();
    let mirrorlist = resolve_mirrorlist(
        held_mirrorlist,
        &arch,
        mirrorlist_for(&arch, &mirrorlist_dir()).await,
    );

    // Best-effort, from the parsed .SRCINFO: the `validpgpkeys` the worker must
    // trust, and the `git+` sources whose commit it should report back. A
    // parse failure costs neither outright -- the worker can still self-extract
    // the keys, and an unreported source leaves the commit recorded at queue
    // time standing.
    let (pgp_keys, vcs_sources) = match store
        .sourceinfo(&pkg.source_data, pkg.patch.as_deref())
        .await
    {
        Ok(si) => (
            si.base
                .pgp_fingerprints
                .iter()
                .map(ToString::to_string)
                .collect(),
            job_vcs_sources(&si),
        ),
        Err(_) => (Vec::new(), Vec::new()),
    };

    let build_flags = pkg.build_flags.as_slice().to_vec();

    // Resolved here rather than on the worker: settings are the server's,
    // with the package overriding the global default.
    let persistent_builddir =
        settings::get(db, settings::key::PERSISTENT_BUILDDIR, Some(build.pkg_id))
            .await
            .value;

    // Before `pkg.name` moves into the descriptor below.
    let packages = aurcache_utils::publish::expected_pkgnames(&pkg);
    Ok(JobDescriptor {
        build_id: build.id,
        pkgbase: pkg.name,
        expected_packages: packages,
        expected_version: build.version.clone(),
        arch,
        build_flags,
        persistent_builddir,
        makepkg_conf,
        pacman_conf,
        mirrorlist,
        pgp_keys,
        vcs_sources,
    })
}

/// Stream the (server-patched) source archive for a claimed job.
#[utoipa::path(get, path = "/worker/jobs/{build_id}/source", responses(
    (status = 200, description = "The package's sources as a gzipped tarball, with the server's patches applied", content_type = "application/gzip"),
    (status = 404, description = "No such build"),
    (status = 403, description = "Not an approved worker, or not the worker that holds this build"),
    (status = 428, description = "This server holds no protocol agreed with the worker: register again, then retry"),
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
        .map_err(|e| err(Status::Forbidden, e))?;
    let pkg = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "package not found"))?;

    store
        .archive_bytes(&pkg.source_data, pkg.patch.as_deref())
        .await
        .map_err(|e| err(Status::InternalServerError, e))
}

/// The pkgbase a build belongs to.
///
/// Worker routes are keyed by the internal build id while logs are stored under
/// `<pkgbase>/<number>`, so the name has to be looked up once per request.
async fn build_pkgbase(db: &DatabaseConnection, pkg_id: i32) -> Result<String, ApiError> {
    Packages::find_by_id(pkg_id)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .map(|p| p.name)
        .ok_or_else(|| err(Status::NotFound, "package not found"))
}

/// Append build log output.
#[utoipa::path(post, path = "/worker/jobs/{build_id}/logs",
    request_body(content = String, description = "Build output to append, as UTF-8 text", content_type = "text/plain"),
    responses(
        (status = 200, description = "Appended"),
        (status = 400, description = "Not text, or the build's log is full"),
    (status = 403, description = "Not an approved worker, or not the worker that holds this build"),
    (status = 428, description = "This server holds no protocol agreed with the worker: register again, then retry"),
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
        .map_err(|e| err(Status::Forbidden, e))?;

    let text = data
        .open(4.mebibytes())
        .into_string()
        .await
        .map_err(|e| err(Status::BadRequest, e))?;

    // Logs are stored under the build's public identity, and the worker
    // endpoints are the only ones keyed by the internal id. `build` comes from
    // the ownership check above, so this costs one lookup for the pkgbase.
    let pkgbase = build_pkgbase(db, build.pkg_id).await?;

    // The worker posts whole chunks, so this needs no buffering task of its own.
    append_build_output(&pkgbase, build.number, &text)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
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
    (status = 403, description = "Not an approved worker, or not the worker that holds this build"),
    (status = 428, description = "This server holds no protocol agreed with the worker: register again, then retry"),
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
                .map_err(|e| err(Status::Forbidden, e))?;
            let pkgbase = build_pkgbase(db, build.pkg_id).await?;
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
/// RAM, which is what made the previous upload path stall and time out under
/// memory pressure.
#[utoipa::path(post, path = "/worker/jobs/{build_id}/artifacts/{filename}",
    request_body(content = Vec<u8>, description = "One built package file, streamed; send Content-Length when known so an oversized one is refused before any of it is sent", content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Staged; published with the rest when the build completes"),
        (status = 400, description = "Not an acceptable artifact name"),
        (status = 413, description = "Larger than this build may produce; the body says which limit"),
    (status = 403, description = "Not an approved worker, or not the worker that holds this build"),
    (status = 428, description = "This server holds no protocol agreed with the worker: register again, then retry"),
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
        .map_err(|e| err(Status::Forbidden, e))?;

    // A backstop against a runaway or hostile PKGBUILD filling the server's
    // disk, not a statement about what a package should weigh -- which is why
    // it is a setting, and per package: `unreal-engine` is past 20 GiB, and
    // allowing it that should not allow everything else the same.
    let limit = settings::get(db, settings::key::MAX_ARTIFACT_SIZE, Some(build.pkg_id)).await;
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
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

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
    (status = 403, description = "Not an approved worker, or not the worker that holds this build"),
    (status = 428, description = "This server holds no protocol agreed with the worker: register again, then retry"),
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
    let outcome = complete_job_inner(
        db.inner(),
        repo.inner(),
        al.inner(),
        &auth,
        build_id,
        input.into_inner(),
    )
    .await;
    if let Err(e) = &outcome {
        tracing::warn!(
            "rejected completion of build {build_id} from worker {}: {} -- {}",
            auth.worker.id,
            e.0,
            e.1
        );
        // A build nobody can name any more -- its package deleted -- leaves
        // only the journal line above.
        if let Ok(named) = aurcache_db::helpers::builds::build_refs(db.inner(), &[build_id]).await
            && let Some(build) = named.into_iter().next()
        {
            al.emit(Event::BuildCompletionRejected {
                build,
                worker: auth.worker.name.as_str().into(),
                reason: e.1.clone(),
            });
        }
    }
    outcome
}

async fn complete_job_inner(
    db: &DatabaseConnection,
    repo: &Arc<Repository>,
    activity: &ActivityLog,
    auth: &WorkerAuth,
    build_id: i32,
    report: CompleteReport,
) -> Result<(), ApiError> {
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "build not found"))?;

    // A report about a build this worker ran that has already moved on is a
    // repeat, and is answered 200 without changing anything: the worker's
    // earlier request got through and only its answer was lost.
    //
    // * A cancelled report acknowledges an abort the server carried out
    //   (operator Stop, or the reaper abandoning the build).
    // * A success repeats a completion that was accepted: the build is being
    //   published, was published, or failed to publish -- which fails with no
    //   end reason, unlike a cancel or an abandonment.
    //
    // A real ownership check: without it, any authenticated worker that
    // guessed an id could remove another worker's staging directory.
    if build.worker_id == Some(auth.worker.id) {
        let failed = build.status == BuildState::Failed;
        // An interruption the server already abandoned the build for is the
        // same: the build left this worker's hands either way.
        let acknowledged_abort = matches!(
            report.outcome,
            BuildOutcome::Canceled | BuildOutcome::Interrupted
        ) && failed;
        let repeated_success = report.outcome == BuildOutcome::Succeeded
            && (matches!(
                build.status,
                BuildState::Publishing | BuildState::Successful
            ) || (failed && build.end_reason.is_none()));
        if acknowledged_abort {
            let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
            return Ok(());
        }
        if repeated_success {
            return Ok(());
        }
    }

    worker_complete::check_owned_active(auth.worker.id, &build)
        .map_err(|e| err(Status::Forbidden, e))?;

    // What the entries below name it by. A package gone from under an active
    // build is not something to refuse the report over.
    let pkgbase = build_pkgbase(db, build.pkg_id).await.ok();
    let named = pkgbase.as_ref().map(|pkgbase| BuildRef {
        pkgbase: pkgbase.clone(),
        number: build.number,
    });
    let worker = || WorkerRef::from(auth.worker.name.as_str());
    let record_failed = |what: &str, error: String| {
        if let Some(build) = &named {
            activity.emit(Event::BuildRecordFailed {
                build: build.clone(),
                what: what.to_string(),
                error,
            });
        }
    };

    // Before the branch below: an OOM-killed build never reaches the success
    // path, and that is the build whose memory figure matters most.
    if let Some(peak) = report.peak_memory_bytes
        && let Err(e) = worker_complete::record_peak_memory(db, build_id, peak).await
    {
        record_failed("peak memory", e.to_string());
    }
    if let Some(usage) = report.disk_usage.as_ref().filter(|u| !u.is_empty())
        && let Err(e) = worker_complete::record_disk_usage(db, build_id, usage).await
    {
        record_failed("disk usage", e.to_string());
    }
    if let Some(kept) = report.kept.as_ref()
        && let Err(e) = worker_complete::record_kept(db, build_id, kept).await
    {
        record_failed("kept chroot", e.to_string());
    }

    if report.outcome == BuildOutcome::Succeeded {
        // What the worker actually checked out replaces what the server
        // guessed when it queued the build. Only from a success, and only when
        // the worker reported something: one that could not resolve a source
        // leaves the queue-time record standing -- the older commit, so the
        // error is a redundant rebuild rather than a missed one.
        if !report.vcs_commits.is_empty()
            && let Err(e) = aurcache_db::helpers::builds::record_build_vcs_sources(
                db,
                build_id,
                &report.vcs_commits,
            )
            .await
        {
            record_failed("built VCS sources", e.to_string());
        }
        worker_complete::accept_for_publishing(db, build_id, auth.worker.id)
            .await
            .map_err(|e| err(Status::Forbidden, e))?;
        if let Some(build) = named.clone() {
            activity.emit(Event::BuildSucceeded {
                build,
                worker: worker(),
            });
        }
        let (db, repo, activity) = (db.clone(), Arc::clone(repo), activity.clone());
        tokio::spawn(async move {
            publish_with_recovery(&db, &repo, &activity, build_id).await;
        });
        return Ok(());
    }

    if let Some(reason) = &report.reason
        && let Some(pkgbase) = &pkgbase
        && let Err(e) = append_build_output(
            pkgbase,
            build.number,
            &format!("worker reported failure: {reason}\n"),
        )
        .await
    {
        record_failed("failure reason", e.to_string());
    }

    if report.outcome == BuildOutcome::Interrupted {
        let abandoned = worker_jobs::abandon_build(
            db,
            &build,
            EndReason::Interrupted,
            worker_policy().max_attempts,
        )
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::Forbidden, "build is no longer this worker's"))?;
        aurcache_utils::abandoned::report(db, activity, &[abandoned]).await;
        let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
        return Ok(());
    }
    // Refused like a success above when the lease was lost in the meantime.
    worker_complete::complete_failure(db, build_id, auth.worker.id)
        .await
        .map_err(|e| err(Status::Forbidden, e))?;
    if let Some(build) = named {
        activity.emit(Event::BuildFailed {
            build,
            worker: worker(),
            reason: report.reason.clone(),
        });
    }
    let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
    Ok(())
}

/// Liveness heartbeat: renews leases for reported builds, abandons any this
/// worker silently dropped, and returns the builds the server wants it to stop
/// (abandoned, or cancelled by an operator). The abort list rides this same
/// answer rather than a second poll, so a cancelled or lost build is stopped on
/// the worker's next heartbeat.
#[utoipa::path(post, path = "/worker/heartbeat", request_body = Heartbeat, responses(
    (status = 200, description = "Leases renewed for the builds listed; the answer names builds to stop and carries new settings", body = HeartbeatResponse),
    (status = 403, description = "Not an approved worker, or not the worker that holds this build"),
    (status = 428, description = "This server holds no protocol agreed with the worker: register again, then retry"),
))]
#[post("/worker/heartbeat", data = "<input>")]
pub async fn heartbeat(
    db: &State<DatabaseConnection>,
    mut auth: WorkerAuth,
    al: &State<ActivityLog>,
    input: Json<Heartbeat>,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    let db = db.inner();
    let hb = input.into_inner();
    worker_store::touch_last_seen(db, &mut auth.worker, Some(&hb.version))
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    // Sent only when it changed, so this is normally absent. A report that
    // cannot be stored must not cost the worker its heartbeat -- the leases
    // this renews are what keep its builds alive.
    if let Some(effective) = &hb.effective {
        let worker = || WorkerRef::from(auth.worker.name.as_str());
        let failed = |error: String| Event::WorkerReportFailed {
            worker: worker(),
            report: WorkerReport::Configuration,
            error,
        };
        match serde_json::to_string(effective) {
            Ok(json) => {
                if let Err(e) =
                    worker_store::store_effective_config(db, auth.worker.id, &json).await
                {
                    al.emit(failed(e.to_string()));
                }
            }
            Err(e) => al.emit(failed(e.to_string())),
        }

        // Against what it last reported. The first report has nothing to
        // differ from, and a restart that comes back the same says nothing.
        if let Some(before) = auth
            .worker
            .effective_config
            .as_deref()
            .and_then(|raw| serde_json::from_str::<EffectiveConfig>(raw).ok())
        {
            let changed = effective.changed_since(&before);
            if !changed.is_empty() {
                al.emit(Event::WorkerConfigChanged {
                    worker: worker(),
                    settings: changed,
                });
            }
        }

        // A worker sends this only when it changes, so this is once per worker
        // process rather than once per heartbeat: the log records a machine
        // coming up running something other than what it was configured with.
        let refused: Vec<String> = effective
            .settings
            .iter()
            .filter(|(_, setting)| setting.status == SettingStatus::Rejected)
            .map(|(key, _)| key.clone())
            .collect();
        if !refused.is_empty() {
            al.emit(Event::WorkerSettingRejected {
                worker: auth.worker.name.clone().into(),
                settings: refused,
            });
        }
    }
    let outcome = worker_jobs::heartbeat(
        db,
        auth.worker.id,
        &hb.active_build_ids,
        worker_policy().lease_ttl_secs,
        worker_policy().max_attempts,
    )
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;
    // Builds the worker stopped listing were abandoned above, each with a
    // fresh attempt while the budget allows. Said the way the reaper says it:
    // they were taken back from this worker.
    if !outcome.dropped.is_empty() {
        let named = aurcache_utils::abandoned::named(&outcome.dropped);
        al.emit(Event::WorkerReaped {
            workers: vec![auth.worker.name.as_str().into()],
            retried: named.retried,
            failed: named.failed,
        });
        aurcache_utils::abandoned::report(db, al, &outcome.dropped).await;
    }
    Ok(Json(HeartbeatResponse {
        cancel: outcome.cancel_requested,
        config: snapshot_for(db, &auth.worker, hb.received_revision.as_deref()).await,
    }))
}

/// The values set for this worker, when it does not hold them yet.
///
/// Read after the leases are renewed and never allowed to fail the heartbeat:
/// a snapshot that cannot be read this time is sent on the next one, while a
/// heartbeat that failed would cost the worker its builds.
async fn snapshot_for(
    db: &DatabaseConnection,
    worker: &workers::Model,
    received: Option<&str>,
) -> Option<ConfigSnapshot> {
    let values = worker_store::worker_setting_values(db, worker.id)
        .await
        .map_err(|e| tracing::warn!("reading worker {}'s settings: {e}", worker.id))
        .ok()?;
    let revision = config_revision(&values);
    (received != Some(revision.as_str())).then_some(ConfigSnapshot {
        revision,
        settings: values,
    })
}

/// Poll whether a cancel was requested for a specific job.
///
/// A build the worker still owns is cancelled when the server moved it out of
/// `ACTIVE` (an operator Stop, or the reaper abandoning it). A build that left
/// this worker's hands entirely — retried, reclaimed by someone else, or
/// already terminal — reads as cancelled too: the worker must stop touching it.
/// Only an `ACTIVE` row owned by *another* worker is refused, because that is
/// the one case where answering would leak a job descriptor the reporter never
/// had. Without row reuse that refusal is unreachable in practice, and 404 does
/// the rest of the withholding.
#[utoipa::path(get, path = "/worker/jobs/{build_id}/status", responses(
    (status = 200, description = "Whether the worker should stop this build", body = JobStatus),
    (status = 404, description = "No such build"),
    (status = 403, description = "Not an approved worker, or not the worker that holds this build"),
    (status = 428, description = "This server holds no protocol agreed with the worker: register again, then retry"),
))]
#[get("/worker/jobs/<build_id>/status")]
pub async fn job_status(
    db: &State<DatabaseConnection>,
    auth: WorkerAuth,
    build_id: i32,
) -> Result<Json<JobStatus>, ApiError> {
    let db = db.inner();
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "build not found"))?;
    let mine = build.worker_id == Some(auth.worker.id);
    if build.status == BuildState::Active && !mine {
        return Err(err(Status::Forbidden, "not owner"));
    }
    // Cancel is signalled by leaving ACTIVE while owned, or by no longer being
    // this worker's to run.
    let cancel_requested = build.status != BuildState::Active || !mine;
    Ok(Json(JobStatus { cancel_requested }))
}

// ----------------------------------------------------------------------------
// Admin management (operator auth)
// ----------------------------------------------------------------------------

/// How many builds a worker is running, and how its finished ones went.
#[derive(Default, Clone, Copy)]
struct BuildTally {
    active: i32,
    successful: i32,
    failed: i32,
}

/// One row per (worker, status) from the builds table.
#[derive(sea_orm::FromQueryResult)]
struct StatusCount {
    worker_id: Option<i32>,
    status: BuildState,
    count: i64,
}

/// Tally every worker's builds in one grouped query.
///
/// One query rather than three per worker: the workers page is a list, and a
/// count per row per outcome is how a list page starts issuing dozens of
/// queries to render.
async fn build_tallies(
    db: &DatabaseConnection,
) -> Result<HashMap<i32, BuildTally>, sea_orm::DbErr> {
    let rows = Builds::find()
        .select_only()
        .column(builds::Column::WorkerId)
        .column(builds::Column::Status)
        .column_as(builds::Column::Id.count(), "count")
        .filter(builds::Column::WorkerId.is_not_null())
        .group_by(builds::Column::WorkerId)
        .group_by(builds::Column::Status)
        .into_model::<StatusCount>()
        .all(db)
        .await?;

    let mut tallies: HashMap<i32, BuildTally> = HashMap::new();
    for row in rows {
        let Some(worker_id) = row.worker_id else {
            continue;
        };
        // Saturating because these are display counts: a repository with more
        // than two billion builds of one worker should render a big number,
        // not panic the list.
        let count = i32::try_from(row.count).unwrap_or(i32::MAX);
        let tally = tallies.entry(worker_id).or_default();
        match row.status {
            BuildState::Active => tally.active = count,
            BuildState::Successful => tally.successful = count,
            BuildState::Failed => tally.failed = count,
            // Queued builds belong to no worker yet, and a publishing one is
            // counted once it has settled.
            BuildState::Enqueued | BuildState::WaitingForDeps | BuildState::Publishing => {}
        }
    }
    Ok(tallies)
}

/// A worker row as the operator's view of it.
///
/// Not the row itself: that carries `signed_cert`, and there is no reason to
/// hand a browser the certificate issued to a build machine.
fn summarise(worker: workers::Model, tally: BuildTally, now: i64, timeout: i64) -> WorkerSummary {
    WorkerSummary {
        id: worker.id,
        name: worker.name,
        status: worker.status,
        cert_fingerprint: worker.cert_fingerprint,
        native_arches: worker.native_arches.into_vec(),
        emulated_arches: worker.emulated_arches.into_vec(),
        package_affinity: worker.package_affinity.into_vec(),
        priority: worker.priority,
        last_seen: worker.last_seen,
        version: worker.version,
        kind: worker.kind,
        online: worker_jobs::is_live(worker.last_seen, now, timeout),
        active_builds: tally.active,
        successful_builds: tally.successful,
        failed_builds: tally.failed,
        settings_rejected: rejected_settings(worker.effective_config.as_deref(), worker.id),
        paused: worker.paused,
    }
}

/// How many settings the worker could not use the configured value for.
///
/// A count rather than the report itself: this rides the workers list, which is
/// polled, and what the list needs is only whether there is something to look
/// at. `None` when the worker has reported nothing -- "not known" and "nothing
/// wrong" are different answers, and the page shows them differently.
fn rejected_settings(effective: Option<&str>, id: i32) -> Option<i32> {
    let report: EffectiveConfig = parse_stored(effective?, id, "configuration report")?;
    i32::try_from(
        report
            .settings
            .values()
            .filter(|setting| setting.status == SettingStatus::Rejected)
            .count(),
    )
    .ok()
}

#[utoipa::path(get, path = "/workers", responses((status = 200, body = [WorkerSummary])))]
#[get("/workers")]
pub async fn list_workers(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<Vec<WorkerSummary>>, ApiError> {
    let db = db.inner();
    // Independent reads: serial awaits would pay both round trips in turn.
    let (workers, tallies) = tokio::join!(worker_store::list_workers(db), build_tallies(db),);
    let workers = workers.map_err(|e| err(Status::InternalServerError, e))?;
    let tallies = tallies.map_err(|e| err(Status::InternalServerError, e))?;
    let now = now_secs();
    let timeout = liveness_timeout_secs();
    Ok(Json(
        workers
            .into_iter()
            .map(|worker| {
                let tally = tallies.get(&worker.id).copied().unwrap_or_default();
                summarise(worker, tally, now, timeout)
            })
            .collect(),
    ))
}

/// The image and port a copy-and-run `docker run` needs, for the Workers page
/// to show one when the fleet is empty. The host is not here on purpose — the
/// browser knows the address the page was reached on and the server does not.
#[utoipa::path(get, path = "/workers/join-info", responses((status = 200, body = WorkerJoinInfo)))]
#[get("/workers/join-info")]
pub async fn worker_join_info(_a: Authenticated) -> Json<WorkerJoinInfo> {
    let image = env::var("AURCACHE_WORKER_IMAGE")
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
/// Both halves are stored as the worker sent them and parsed here. A stored
/// blob that no longer parses is reported as absent rather than failing the
/// request: it means a worker newer than this server in a way the shared
/// vocabulary did not cover, and the rest of the page is still worth showing.
#[utoipa::path(get, path = "/workers/{id}/config", responses((status = 200, body = WorkerConfigView)))]
#[get("/workers/<id>/config")]
pub async fn worker_config(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
    id: i32,
) -> Result<Json<WorkerConfigView>, ApiError> {
    let db = db.inner();
    let worker = find_or_404(db, id).await?;
    config_view(db, worker).await.map(Json)
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
    responses((status = 200, body = WorkerConfigView))
)]
#[patch("/workers/<id>/config", data = "<input>")]
pub async fn update_worker_config(
    db: &State<DatabaseConnection>,
    a: Authenticated,
    al: &State<ActivityLog>,
    id: i32,
    input: Json<WorkerConfigUpdate>,
) -> Result<Json<WorkerConfigView>, ApiError> {
    let db = db.inner();
    let worker = find_or_404(db, id).await?;
    let changes = checked_changes(&declaration(&worker), input.into_inner().settings)
        .map_err(|e| err(Status::BadRequest, e))?;

    let saved = worker_store::save_worker_settings(db, id, &changes)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    if !saved.is_empty() {
        al.emit_by(
            Event::WorkerSettingsSaved {
                worker: worker.name.as_str().into(),
                set: saved.set,
                reset: saved.reset,
            },
            a.username,
        );
    }
    config_view(db, worker).await.map(Json)
}

/// Check a save against the worker's declaration, and put each value in the
/// form it is stored in.
///
/// All of it or none: every refusal is collected, so an operator who got two
/// fields wrong hears about both rather than fixing them one round trip at a
/// time.
fn checked_changes(
    declared: &[SettingDecl],
    changes: BTreeMap<String, Option<String>>,
) -> Result<BTreeMap<String, Option<String>>, String> {
    let mut refused = Vec::new();
    let mut checked = BTreeMap::new();
    for (key, value) in changes {
        let value = match value {
            // Removing needs no declaration: a value for a key the worker has
            // since dropped is exactly the one worth being able to clear.
            None => None,
            Some(raw) => {
                let value = raw.trim().to_string();
                // Blank is how a worker's environment spells "unset", so a
                // stored blank would read as nothing at all.
                let problem = if value.is_empty() {
                    Some(format!(
                        "{key}: remove the value rather than saving it empty"
                    ))
                } else {
                    validate_value(declared, &key, &value)
                        .err()
                        .map(|why| format!("{key}: {why}"))
                };
                if let Some(problem) = problem {
                    refused.push(problem);
                    continue;
                }
                Some(value)
            }
        };
        checked.insert(key, value);
    }
    if refused.is_empty() {
        Ok(checked)
    } else {
        refused.dedup();
        Err(refused.join("; "))
    }
}

async fn find_or_404(db: &DatabaseConnection, id: i32) -> Result<workers::Model, ApiError> {
    worker_store::find_worker(db, id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "no such worker"))
}

/// A worker's configuration as the page shows it: what it accepts, what it
/// runs, and what is set for it here.
async fn config_view(
    db: &DatabaseConnection,
    worker: workers::Model,
) -> Result<WorkerConfigView, ApiError> {
    let id = worker.id;
    let values = worker_store::worker_setting_values(db, id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(WorkerConfigView {
        worker_id: id,
        revision: config_revision(&values),
        settings: declaration(&worker),
        effective: worker
            .effective_config
            .as_deref()
            .and_then(|json| parse_stored(json, id, "configuration report")),
        values,
    })
}

/// What `worker` declared it accepts; nothing, when the stored declaration
/// cannot be read.
fn declaration(worker: &workers::Model) -> Vec<SettingDecl> {
    parse_stored(&worker.settings_declaration, worker.id, "declaration").unwrap_or_default()
}

/// Read one of the stored worker-configuration blobs, saying so rather than
/// failing when it cannot be read.
fn parse_stored<T: serde::de::DeserializeOwned>(json: &str, id: i32, what: &str) -> Option<T> {
    serde_json::from_str(json)
        .map_err(|e| {
            tracing::warn!("worker {id}'s stored {what} could not be read: {e}");
        })
        .ok()
}

#[utoipa::path(post, path = "/workers/{id}/approve", responses((status = 200)))]
#[post("/workers/<id>/approve")]
pub async fn approve_worker(
    db: &State<DatabaseConnection>,
    a: Authenticated,
    al: &State<ActivityLog>,
    id: i32,
) -> Result<(), ApiError> {
    let db = db.inner();
    let worker = worker_store::approve_worker(db, id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "no such worker"))?;
    al.emit_by(
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
    db: &State<DatabaseConnection>,
    a: Authenticated,
    al: &State<ActivityLog>,
    id: i32,
) -> Result<(), ApiError> {
    set_paused(db.inner(), a, al, id, true).await
}

/// Let a paused worker take builds again.
#[utoipa::path(post, path = "/workers/{id}/resume", responses((status = 200)))]
#[post("/workers/<id>/resume")]
pub async fn resume_worker(
    db: &State<DatabaseConnection>,
    a: Authenticated,
    al: &State<ActivityLog>,
    id: i32,
) -> Result<(), ApiError> {
    set_paused(db.inner(), a, al, id, false).await
}

async fn set_paused(
    db: &DatabaseConnection,
    a: Authenticated,
    al: &ActivityLog,
    id: i32,
    paused: bool,
) -> Result<(), ApiError> {
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
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "no such worker"))?;
    // Asking for what already holds changes nothing, and logs nothing.
    if before.paused != paused {
        let worker = worker.name.into();
        let event = if paused {
            Event::WorkerPaused { worker }
        } else {
            Event::WorkerResumed { worker }
        };
        al.emit_by(event, a.username);
    }
    Ok(())
}

#[utoipa::path(post, path = "/workers/{id}/revoke", responses((status = 200)))]
#[post("/workers/<id>/revoke")]
pub async fn revoke_worker(
    db: &State<DatabaseConnection>,
    a: Authenticated,
    al: &State<ActivityLog>,
    id: i32,
) -> Result<(), ApiError> {
    let db = db.inner();
    let revoked = worker_store::revoke_worker(db, id, worker_policy().max_attempts)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "no such worker"))?;
    al.emit_by(
        Event::WorkerRevoked {
            worker: revoked.worker.name.into(),
            requeued: aurcache_utils::abandoned::named(&revoked.abandoned).retried,
        },
        a.username,
    );
    aurcache_utils::abandoned::report(db, al, &revoked.abandoned).await;
    Ok(())
}

#[cfg(test)]
mod repo_template_tests {
    use super::{MirrorlistOffer, render_repo_template};
    use aurcache_common::worker::MirrorlistPreference;
    use std::collections::BTreeMap;

    fn holding(arch: &str, checksum: &str) -> MirrorlistPreference {
        let mut checksums = BTreeMap::new();
        checksums.insert(arch.to_string(), checksum.to_string());
        MirrorlistPreference::Server { checksums }
    }

    /// The checksum a worker sent `list` would hold.
    fn checksum_sent_with(list: &str) -> String {
        match super::resolve_mirrorlist(
            &MirrorlistPreference::Server {
                checksums: BTreeMap::new(),
            },
            "x86_64",
            Some(list.to_string()),
        ) {
            MirrorlistOffer::Send { checksum, .. } => checksum,
            other => panic!("expected the list to be sent, got {other:?}"),
        }
    }

    /// A worker that holds nothing is sent the content, with the checksum it
    /// should echo back next time.
    #[test]
    fn a_worker_holding_nothing_is_sent_the_mirrorlist() {
        let offer = super::resolve_mirrorlist(
            &MirrorlistPreference::Server {
                checksums: BTreeMap::new(),
            },
            "x86_64",
            Some("Server = http://mirror/\n".to_string()),
        );
        assert!(
            matches!(&offer, MirrorlistOffer::Send { content, .. } if content == "Server = http://mirror/\n"),
            "{offer:?}"
        );
    }

    /// The point of the whole exchange: matching checksum, no bytes resent.
    #[test]
    fn a_matching_checksum_withholds_the_content() {
        let list = "Server = http://mirror/\n".to_string();
        let checksum = checksum_sent_with(&list);
        let offer = super::resolve_mirrorlist(&holding("x86_64", &checksum), "x86_64", Some(list));
        assert_eq!(offer, MirrorlistOffer::Keep);
    }

    /// A checksum held for one architecture says nothing about another, which
    /// is why the claim carries a map rather than a single value.
    #[test]
    fn a_checksum_for_another_arch_does_not_match() {
        let list = "Server = http://mirror/\n".to_string();
        let checksum = checksum_sent_with(&list);
        let offer = super::resolve_mirrorlist(&holding("x86_64", &checksum), "aarch64", Some(list));
        assert!(
            matches!(offer, MirrorlistOffer::Send { .. }),
            "aarch64 has not been sent this list"
        );
    }

    /// "The server has none" must reach the worker as something other than
    /// "keep what you have": it has to drop a stale entry rather than reuse it.
    #[test]
    fn no_server_mirrorlist_is_not_reported_as_unchanged() {
        let offer = super::resolve_mirrorlist(&holding("x86_64", "whatever"), "x86_64", None);
        assert_eq!(offer, MirrorlistOffer::Nothing);
    }

    /// A worker with its own mirrorlist is sent nothing at all.
    #[test]
    fn a_local_worker_is_sent_nothing() {
        let offer = super::resolve_mirrorlist(
            &MirrorlistPreference::Local,
            "x86_64",
            Some("Server = http://mirror/\n".to_string()),
        );
        assert_eq!(offer, MirrorlistOffer::Nothing);
    }

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

#[cfg(test)]
mod config_tests {
    use super::{checked_changes, config_revision};
    use aurcache_common::worker_config::{Applies, SettingDecl, ValueKind};
    use std::collections::BTreeMap;

    fn declared() -> Vec<SettingDecl> {
        vec![SettingDecl {
            key: "build_memory_max".to_string(),
            kind: ValueKind::Size,
            description: String::new(),
            category: String::new(),
            default: None,
            env_var: Some("WORKER_BUILD_MEMORY_MAX".to_string()),
            applies: Applies::NextJob,
        }]
    }

    fn save(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.map(str::to_string)))
            .collect()
    }

    /// A value is stored the way it will be compared: trimmed, so a stray
    /// space does not make the same value a different revision.
    #[test]
    fn a_valid_value_is_stored_trimmed() {
        let checked =
            checked_changes(&declared(), save(&[("build_memory_max", Some(" 32G "))])).unwrap();
        assert_eq!(checked["build_memory_max"].as_deref(), Some("32G"));
    }

    /// Every refusal is reported at once, and nothing is taken: a save is
    /// whole or not at all.
    #[test]
    fn every_refusal_is_reported_together() {
        let why = checked_changes(
            &declared(),
            save(&[
                ("build_memory_max", Some("lots")),
                ("build_user", Some("root")),
            ]),
        )
        .unwrap_err();
        assert!(why.contains("build_memory_max"), "{why}");
        assert!(why.contains("build_user"), "{why}");
    }

    /// A blank value would read as unset on the worker, so it is refused
    /// rather than stored as a value that does nothing.
    #[test]
    fn a_blank_value_is_refused() {
        assert!(checked_changes(&declared(), save(&[("build_memory_max", Some("  "))])).is_err());
    }

    /// Removing needs no declaration: a value for a key the worker has since
    /// dropped, or one set before it was upgraded away, must stay clearable.
    #[test]
    fn a_removal_needs_no_declaration() {
        let checked = checked_changes(&[], save(&[("gone", None)])).unwrap();
        assert_eq!(checked["gone"], None);
        let refused = checked_changes(&[], save(&[("build_memory_max", Some("32G"))]));
        assert!(
            refused.is_err(),
            "a worker that declares nothing cannot take a value"
        );
    }

    /// The revision follows the values alone: equal sets agree whatever order
    /// they were built in, and any change moves it.
    #[test]
    fn the_revision_follows_the_values() {
        let a = BTreeMap::from([
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]);
        let b = BTreeMap::from([
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "1".to_string()),
        ]);
        assert_eq!(config_revision(&a), config_revision(&b));
        let mut c = a.clone();
        c.insert("a".to_string(), "3".to_string());
        assert_ne!(config_revision(&a), config_revision(&c));
        assert_ne!(config_revision(&a), config_revision(&BTreeMap::new()));
    }
}
