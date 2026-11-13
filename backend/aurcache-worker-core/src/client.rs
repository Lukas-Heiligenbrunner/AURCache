//! HTTPS/mTLS client for the worker protocol.
//!
//! Two construction paths:
//! * [`WorkerClient::enrollment`] — trusts the server via a pinned CA
//!   fingerprint (or TOFU on first contact) and carries no client certificate;
//!   used for `register` / status polling / CA fetch.
//! * [`WorkerClient::authenticated`] — presents the CA-signed client
//!   certificate (mTLS) and validates the server against the pinned CA; used
//!   for all job endpoints.

use crate::protocol::PackagesRefused;
use crate::repo::RepoSection;
use anyhow::{Context, Result, bail};
use aurcache_common::api::activity::Severity;
use aurcache_common::worker::{
    ClaimRequest, CompleteReport, Heartbeat, HeartbeatResponse, JobDescriptor, RegisterRequest,
    RegisterStatus, WorkerLogReport,
};
use percent_encoding::{AsciiSet, CONTROLS};
use reqwest::{Body, Certificate, Client, Identity, RequestBuilder, Response, StatusCode};
use std::time::Duration;
use tokio_util::io::ReaderStream;

/// Characters that must not appear raw in a URL path segment. Everything legal
/// in a real makepkg artifact name (`.`, `-`, `_`, `+`, `:`, `~`) is left as-is
/// so URLs stay readable in logs.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b'\\')
    .add(b'%');

/// How long to wait for a TCP+TLS connection to establish before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a request may wait for its answer. A silently stalled connection
/// trips this well before the lease TTL (default 60s), so the worker's lease
/// self-abort watchdog is never defeated by a hung socket.
///
/// In reqwest this is not an idle timeout while a request is under way: it is
/// a deadline from sending the request to receiving the response headers, and
/// streaming the request body does not reset it (only reading a response body
/// does, per read). Which is why artifact uploads do not use it -- see
/// `WorkerClient::upload_http`.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How often an idle connection is probed. What notices a dead server during
/// an artifact upload, which has no response deadline to do it.
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

/// Sending a request and requiring a success.
trait SendChecked {
    /// Send, failing on a transport error or a status other than success.
    ///
    /// A refusal carries the server's own reason: the server explains what it
    /// refuses in the body, and `error_for_status` would drop that, leaving a
    /// bare "403 Forbidden" with nothing about which check failed.
    async fn send_checked(self, what: &str) -> Result<reqwest::Response>;
}

impl SendChecked for RequestBuilder {
    async fn send_checked(self, what: &str) -> Result<Response> {
        let response = self
            .send()
            .await
            .with_context(|| format!("{what} request"))?;
        checked(response, what).await
    }
}

/// `response` if it is a success, or a [`Refused`] carrying the server's reason.
async fn checked(response: Response, what: &str) -> Result<Response> {
    let status = response.status();
    if status.is_success() || status == StatusCode::NOT_MODIFIED {
        return Ok(response);
    }
    Err(Refused::new(what, status, reason(response).await).into())
}

/// The server answered a request, and the answer was no.
///
/// Typed, unlike a transport error, so that a caller deciding whether to ask
/// again can tell the server's verdict from a request that never got one.
#[derive(Debug)]
pub struct Refused {
    what: String,
    pub status: StatusCode,
    reason: String,
}

impl Refused {
    pub(crate) fn new(what: &str, status: StatusCode, reason: impl Into<String>) -> Self {
        Self {
            what: what.to_string(),
            status,
            reason: reason.into(),
        }
    }

    /// Whether asking again cannot change the answer: the request itself was
    /// refused (the build is gone, or no longer this worker's), not the
    /// server's ability to take it.
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.status.is_client_error()
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} rejected: {}: {}",
            self.what, self.status, self.reason
        )
    }
}

impl std::error::Error for Refused {}

/// The text a refusal carries.
///
/// The body read can fail too (connection dropped mid-error); say that
/// instead of blaming the server with an empty reason.
async fn reason(response: Response) -> String {
    match response.text().await {
        Ok(body) => body.trim().to_string(),
        Err(e) => format!("<error body unreadable: {e}>"),
    }
}

/// The server refused to register this worker: the two share no worker
/// protocol. The text is the server's, naming what each side speaks.
///
/// Waiting fixes it once the server (or this worker) is upgraded, so it is
/// retried, slowly; see `enroll::enroll`.
#[derive(Debug)]
pub struct Incompatible(pub String);

impl std::fmt::Display for Incompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server refused this worker's protocol: {}", self.0)
    }
}

impl std::error::Error for Incompatible {}

/// A configured protocol client bound to a base URL.
pub struct WorkerClient {
    http: Client,
    /// The same client with no response deadline, for artifact uploads.
    ///
    /// A multi-gigabyte package takes minutes to send, and the server answers
    /// only once it has all of it, so [`READ_TIMEOUT`] would fail every upload
    /// longer than thirty seconds while bytes were still flowing.
    upload_http: Client,
    base: String,
    /// The `[repo]` section rendered for this worker from the server's
    /// template. `None` when this server publishes no repository, or no host
    /// could be determined -- and always on an enrollment client, which has
    /// no build to give it.
    repo: Option<RepoSection>,
    /// The last registration the server accepted from this client, sent again
    /// when the server asks for it (see [`Self::exchange`]).
    registration: std::sync::Mutex<Option<RegisterRequest>>,
    /// The lease timing the server answered the last registration with.
    /// Replaced on every registration, including the ones [`Self::exchange`]
    /// makes when the server restarted, since a restarted server may have
    /// been given other timing. `None` only on an enrollment client that has
    /// not registered.
    lease: std::sync::Mutex<Option<Lease>>,
}

/// The lease timing a worker keeps to: the server's, from a registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease {
    /// How long a claimed build's lease lasts without a heartbeat.
    pub ttl: Duration,
    /// How often to send a heartbeat. Never zero.
    pub heartbeat_interval: Duration,
}

impl Lease {
    /// The timing `status` gives.
    #[must_use]
    pub fn of(status: &RegisterStatus) -> Self {
        Self {
            ttl: Duration::from_secs(status.lease_ttl_secs),
            heartbeat_interval: Duration::from_secs(status.heartbeat_interval_secs.max(1)),
        }
    }
}

/// Fetch the server CA (PEM + fingerprint) using a deliberately
/// unauthenticated transport, verifying the fingerprint against `pin` when
/// provided (trust-on-first-use otherwise). Returns the CA PEM.
///
/// The client is built per call on purpose: it accepts invalid certificates
/// (pinning replaces the check), so sharing or hoisting it would risk the one
/// transport that must never validate leaking into requests that must.
pub async fn fetch_and_pin_ca(base: &str, pin: Option<&str>) -> Result<String> {
    let insecure = Client::builder()
        .danger_accept_invalid_certs(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .context("building bootstrap client")?;

    let ca_pem = insecure
        .get(format!("{base}/api/worker/ca"))
        .send_checked("CA certificate")
        .await?
        .text()
        .await
        .context("reading CA certificate")?;

    let actual = ca_fingerprint(&ca_pem)?;
    if let Some(expected) = pin {
        let expected = aurcache_ca::normalize_fingerprint(expected);
        if actual != expected {
            bail!(
                "server CA fingerprint mismatch: expected {expected}, got {actual} \
                 (possible man-in-the-middle)"
            );
        }
        tracing::info!("Verified server CA fingerprint {actual}");
    } else {
        tracing::warn!(
            "Trusting server CA on first use (fingerprint {actual}); \
             set AURCACHE_SERVER_CA_FINGERPRINT to pin it"
        );
    }
    Ok(ca_pem)
}

/// SHA-256 fingerprint of a PEM certificate: the same function the server
/// logs its CA fingerprint with, so a pinned value always matches.
fn ca_fingerprint(pem: &str) -> Result<String> {
    aurcache_ca::cert_fingerprint(pem).context("decoding CA PEM")
}

impl WorkerClient {
    /// The `[repo]` section this worker appends to job configs, if any.
    #[must_use]
    pub fn repo(&self) -> Option<&RepoSection> {
        self.repo.as_ref()
    }

    /// Build an enrollment client that trusts only the pinned CA and carries no
    /// client identity (job endpoints will 401/403 until authenticated).
    pub fn enrollment(base: &str, ca_pem: &str) -> Result<Self> {
        let ca = Certificate::from_pem(ca_pem.as_bytes()).context("parsing CA cert")?;
        let http = Client::builder()
            .use_rustls_tls()
            .add_root_certificate(ca)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .context("building enrollment client")?;
        Ok(Self {
            repo: None,
            // An enrolling worker has no job to upload for.
            upload_http: http.clone(),
            http,
            base: base.to_string(),
            registration: std::sync::Mutex::new(None),
            lease: std::sync::Mutex::new(None),
        })
    }

    /// Build an authenticated mTLS client presenting the worker certificate.
    ///
    /// `registration` is the one the server accepted, sent again whenever it
    /// asks this worker to register; `lease` the timing the server answered
    /// it with; `repo` the section its builds use. All are required here
    /// rather than set afterwards, so no path to a client can leave one out.
    pub fn authenticated(
        base: &str,
        identity: &ClientIdentity<'_>,
        registration: RegisterRequest,
        lease: Lease,
        repo: Option<RepoSection>,
    ) -> Result<Self> {
        let ClientIdentity {
            ca_pem,
            cert_pem,
            key_pem,
        } = *identity;
        let ca = Certificate::from_pem(ca_pem.as_bytes()).context("parsing CA cert")?;
        let mut identity_pem = cert_pem.as_bytes().to_vec();
        identity_pem.extend_from_slice(key_pem.as_bytes());
        let identity =
            Identity::from_pem(&identity_pem).context("building client identity from PEM")?;
        let builder = || {
            Client::builder()
                .use_rustls_tls()
                .add_root_certificate(ca.clone())
                .identity(identity.clone())
                .connect_timeout(CONNECT_TIMEOUT)
                .tcp_keepalive(TCP_KEEPALIVE)
        };
        let http = builder()
            .read_timeout(READ_TIMEOUT)
            .build()
            .context("building authenticated client")?;
        let upload_http = builder()
            .build()
            .context("building authenticated upload client")?;
        Ok(Self {
            repo,
            upload_http,
            http,
            base: base.to_string(),
            registration: std::sync::Mutex::new(Some(registration)),
            lease: std::sync::Mutex::new(Some(lease)),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/worker{path}", self.base)
    }

    /// Register (or re-register) this worker, returning the current status.
    ///
    /// Fails with [`Incompatible`] when the server speaks no protocol this
    /// worker does. Once accepted, the request is what this client sends again
    /// when the server asks it to register (see [`Self::exchange`]).
    pub async fn register(&self, req: &RegisterRequest) -> Result<RegisterStatus> {
        let response = self
            .http
            .post(self.url("/register"))
            .json(req)
            .send()
            .await
            .context("register request")?;
        if response.status() == StatusCode::UPGRADE_REQUIRED {
            return Err(Incompatible(reason(response).await).into());
        }
        let status: RegisterStatus = checked(response, "register")
            .await?
            .json()
            .await
            .context("decoding register status")?;
        self.remember_registration(req.clone());
        *self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Lease::of(&status));
        Ok(status)
    }

    /// The lease timing the server answered this client's latest
    /// registration with.
    ///
    /// Always `Some` on an authenticated client, which is built from one.
    #[must_use]
    pub fn lease(&self) -> Option<Lease> {
        *self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take `req` as the registration to send again when the server asks.
    fn remember_registration(&self, req: RegisterRequest) {
        *self
            .registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(req);
    }

    /// The certificate request of the registration this client holds, for
    /// registering again with changed settings.
    #[must_use]
    pub fn registered_csr(&self) -> Option<String> {
        self.registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|req| req.csr_pem.clone())
    }

    /// Send a request on the authenticated plane.
    ///
    /// A server that holds no protocol agreed with this worker answers `428
    /// Precondition Required` -- after a restart it holds none -- and this
    /// registers again and sends the request once more. A request whose body
    /// was a stream cannot be sent again; that fails, after registering, for
    /// the caller's own retry to succeed.
    async fn exchange(&self, request: RequestBuilder, what: &str) -> Result<Response> {
        let again = request.try_clone();
        let response = request
            .send()
            .await
            .with_context(|| format!("{what} request"))?;
        if response.status() != StatusCode::PRECONDITION_REQUIRED {
            return Ok(response);
        }
        tracing::info!("The server asked this worker to register again");
        let registration = self
            .registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .context(
                "the server asked this worker to register, and it has no registration to send",
            )?;
        self.register(&registration)
            .await
            .context("registering again")?;
        match again {
            Some(again) => again
                .send()
                .await
                .with_context(|| format!("{what} request")),
            None => bail!("{what} was refused until this worker registered again, which it has"),
        }
    }

    /// [`Self::exchange`], requiring a success.
    async fn exchange_checked(&self, request: RequestBuilder, what: &str) -> Result<Response> {
        checked(self.exchange(request, what).await?, what).await
    }

    /// Poll enrollment status by fingerprint, registering again first if the
    /// server restarted since this client last did (see [`Self::exchange`]).
    pub async fn register_status(&self, fingerprint: &str) -> Result<RegisterStatus> {
        let resp = self
            .exchange_checked(
                self.http
                    .get(self.url(&format!("/register/{fingerprint}/status"))),
                "status",
            )
            .await?;
        resp.json().await.context("decoding register status")
    }

    /// Claim the next job for the given arches, or `None` if the queue is empty.
    pub async fn claim(&self, req: &ClaimRequest) -> Result<Option<JobDescriptor>> {
        let resp = self
            .exchange_checked(self.http.post(self.url("/jobs/claim")).json(req), "claim")
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(Some(resp.json().await.context("decoding job")?)),
            StatusCode::NO_CONTENT => Ok(None),
            other => bail!("claim answered {other}, neither a job nor none"),
        }
    }

    /// Download the (server-patched) source tarball bytes for a build.
    pub async fn source(&self, build_id: i32) -> Result<Vec<u8>> {
        let resp = self
            .exchange_checked(
                self.http.get(self.url(&format!("/jobs/{build_id}/source"))),
                "source",
            )
            .await?;
        Ok(resp.bytes().await.context("reading source")?.to_vec())
    }

    /// Append a chunk of build log output.
    pub async fn append_log(&self, build_id: i32, text: &str) -> Result<()> {
        self.exchange_checked(
            self.http
                .post(self.url(&format!("/jobs/{build_id}/logs")))
                .body(text.to_string()),
            "log",
        )
        .await?;
        Ok(())
    }

    /// Report a problem the worker noticed on its own account -- not sent for
    /// a protocol failure the server already finds out about from the same
    /// silence that would swallow this too (a lost claim, a missed
    /// heartbeat), but for something local the server otherwise never learns:
    /// a build that needed a fallback to finish, or maintenance that could
    /// not complete cleanly.
    pub async fn report_problem(
        &self,
        severity: Severity,
        build_id: Option<i32>,
        message: &str,
    ) -> Result<()> {
        self.exchange_checked(
            self.http.post(self.url("/log")).json(&WorkerLogReport {
                severity,
                build_id,
                message: message.to_string(),
            }),
            "worker log",
        )
        .await?;
        Ok(())
    }

    /// Upload a single artifact file, streaming its contents from `reader` so
    /// multi-gigabyte packages are never held in memory on the worker.
    ///
    /// `filename` is percent-encoded: it originates from whatever the PKGBUILD
    /// dropped in the build directory, and an unescaped `?` or `#` would be
    /// parsed as a query/fragment separator and silently truncate the path.
    ///
    /// `len` is sent as `Content-Length` when known, so a server that will not
    /// take a file that size says so before any of it is sent rather than after
    /// reading up to its limit. A refusal carries the server's own reason -- a
    /// bare "413 Payload Too Large" says nothing about which limit, or where
    /// it is set -- and is a [`PackagesRefused`] when it is about the file
    /// (`400`, `413`) rather than about this worker.
    pub async fn upload_artifact<R>(
        &self,
        build_id: i32,
        filename: &str,
        reader: R,
        len: Option<u64>,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
    {
        let encoded = percent_encoding::utf8_percent_encode(filename, PATH_SEGMENT);
        let body = Body::wrap_stream(ReaderStream::new(reader));
        let mut request = self
            .upload_http
            .post(self.url(&format!("/jobs/{build_id}/artifacts/{encoded}")))
            .body(body);
        if let Some(len) = len {
            request = request.header(reqwest::header::CONTENT_LENGTH, len);
        }
        let response = self.exchange(request, "artifact").await?;
        let status = response.status();
        if matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
        ) {
            let reason = reason(response).await;
            return Err(PackagesRefused(format!("artifact refused: {status}: {reason}")).into());
        }
        checked(response, "artifact").await?;
        Ok(())
    }

    /// Report a build's terminal result.
    pub async fn complete(&self, build_id: i32, report: &CompleteReport) -> Result<()> {
        self.exchange_checked(
            self.http
                .post(self.url(&format!("/jobs/{build_id}/complete")))
                .json(report),
            "complete",
        )
        .await?;
        Ok(())
    }

    /// Send a liveness heartbeat listing the builds still running, and return
    /// the server's answer: the builds it wants this worker to stop (abandoned,
    /// or cancelled by an operator), and settings it wants changed.
    pub async fn heartbeat(&self, hb: &Heartbeat) -> Result<HeartbeatResponse> {
        let resp = self
            .exchange_checked(self.http.post(self.url("/heartbeat")).json(hb), "heartbeat")
            .await?;
        // An answer that cannot be read is a failed heartbeat: it must not
        // refresh the lease watchdog.
        resp.json().await.context("decoding heartbeat answer")
    }

    /// Conditional GET of a resource outside the `/api/worker` mount — the
    /// repository DB, which lives on the same server but on the repo port.
    ///
    /// A `304 Not Modified` returns a `None` body meaning "your copy is
    /// current"; a `200` returns the new bytes together with the `ETag` and
    /// `Last-Modified` they came with, so a caller can replay them next time.
    /// Anything else is an error: the caller must treat that as "nothing
    /// authoritative known", never as "reuse what you had".
    pub async fn get_conditional(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<ConditionalGet> {
        let mut req = self.http.get(url);
        if let Some(etag) = etag {
            req = req.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = last_modified {
            req = req.header(reqwest::header::IF_MODIFIED_SINCE, last_modified);
        }
        let resp = req.send_checked("conditional GET").await?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(ConditionalGet {
                body: None,
                etag: None,
                last_modified: None,
            });
        }
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let last_modified = resp
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = resp
            .bytes()
            .await
            .context("reading conditional GET body")?
            .to_vec();
        Ok(ConditionalGet {
            body: Some(body),
            etag,
            last_modified,
        })
    }
}

/// The certificates a worker presents and trusts on the authenticated plane.
#[derive(Clone, Copy)]
pub struct ClientIdentity<'a> {
    /// The server's CA, pinned at enrollment.
    pub ca_pem: &'a str,
    /// This worker's certificate, signed by that CA.
    pub cert_pem: &'a str,
    /// This worker's private key.
    pub key_pem: &'a str,
}

/// Result of a conditional GET: a `None` body means the server sent `304`, so
/// the caller's stored copy is still authoritative.
pub struct ConditionalGet {
    /// The bytes of a `200` response; `None` for `304`.
    pub body: Option<Vec<u8>>,
    /// The `ETag` the bytes came back with, for the next conditional request.
    pub etag: Option<String>,
    /// The `Last-Modified` the bytes came back with, ditto.
    pub last_modified: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CA fingerprint is the hash of the certificate block's body.
    #[test]
    fn a_ca_fingerprint_hashes_the_certificate_block() {
        // "foobar" base64 wrapped as a fake cert block.
        let pem = "-----BEGIN CERTIFICATE-----\nZm9vYmFy\n-----END CERTIFICATE-----\n";
        assert_eq!(
            ca_fingerprint(pem).unwrap(),
            "c3ab8ff13720e8ad9047dd39466b3c8974e592c2fa383d4a3960714caef0c4f2"
        );
    }

    /// The label is the point: a private-key block must not be fingerprinted
    /// as a certificate and fail obscurely when the pin never matches.
    #[test]
    fn a_ca_fingerprint_refuses_other_blocks() {
        let key = "-----BEGIN PRIVATE KEY-----\nZm9vYmFy\n-----END PRIVATE KEY-----\n";
        assert!(ca_fingerprint(key).is_err());
        assert!(ca_fingerprint("not a pem").is_err());
    }
}
