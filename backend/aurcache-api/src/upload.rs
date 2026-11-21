//! Uploading packages built elsewhere (see `aurcache_utils::upload`).

use crate::auth::Authenticated;
use crate::utils::error::{ApiError, err};
use aurcache_common::api::package::UploadedBuild;
use aurcache_utils::services::Services;
use aurcache_utils::settings;
use aurcache_utils::upload::{self, Received, UploadError, UploadRequest};
use pacman_mirrors::platforms::Platform;
use rocket::data::{ByteUnit, DataStream};
use rocket::http::{ContentType, Status};
use rocket::serde::json::Json;
use rocket::{Data, State, post};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};
use utoipa::OpenApi;

/// How much of an upload's body may be anything but the files themselves:
/// the boundaries and part headers, and what the parser has read ahead of
/// the file it is on. A few hundred bytes per part in practice.
///
/// The parser holds all of it in memory while it looks for the end of a part
/// header, so without this a body that never ends one would grow the server
/// until it is killed.
const FRAMING_LIMIT: u64 = 8 << 20;

/// The most of a refused upload's body that is still read, and dropped, so
/// that the client reads the refusal rather than a broken connection (see
/// [`discard_rest`]). More than most packages, so it covers the rest of most
/// uploads; one with more left to send is cut off.
const DISCARD_LIMIT: u64 = 1 << 30;

#[derive(OpenApi)]
#[openapi(paths(upload_package))]
pub struct UploadApi;

impl From<UploadError> for ApiError {
    fn from(e: UploadError) -> Self {
        let status = match &e {
            UploadError::Refused(_) => Status::BadRequest,
            UploadError::Busy(_) | UploadError::Contended(_) => Status::Conflict,
            UploadError::Internal(_) => Status::InternalServerError,
        };
        err(status, e)
    }
}

/// Publish packages built elsewhere as a build of the package `pkgbase`.
///
/// The body is `multipart/form-data`, one part per file, each named by its
/// filename: the package files of one build (split packages included) and,
/// optionally, a detached `.sig` for each. The files' names and sizes are
/// checked before anything is recorded -- each package name against the ones
/// the package's source builds as it arrives, one version and one platform
/// across the upload -- and one file refused refuses the upload. What the
/// files hold is checked when the build is published, as a worker's is: a
/// `.PKGINFO` that is not what its filename says, or an armored `.sig`, fails
/// the build rather than refusing the upload. A refusal is answered once the
/// rest of the body is read and dropped, up to 1 GiB, so a client still
/// sending it reads the answer; one with more left to send is cut off. A request without a valid token is not
/// drained: the auth guard refuses it before the body is touched, so a client
/// sending a large body sees the connection close rather than the `401`.
///
/// A package that does not exist is created as an uploaded package, which no
/// worker ever builds; its files must name it as their pkgbase. No package
/// name an upload publishes may be one another package publishes on that
/// platform.
///
/// Answers once the first attempt to publish has run: `Successful`, or
/// `Publishing` when it continues in the background, or `Failed` with the
/// reason in the build's log.
#[utoipa::path(
    post,
    path = "/package/{pkgbase}/upload",
    request_body(content = Vec<u8>, description = "The package files, one multipart part each, named by filename", content_type = "multipart/form-data"),
    params(
        ("pkgbase", description = "pkgbase of the package; created when there is none"),
        ("platform", Query, description = "The platform an upload of only `arch = any` packages is for; otherwise the files say"),
    ),
    responses(
        (status = 200, description = "The build the upload became", body = UploadedBuild),
        (status = 400, description = "A file that cannot be published under this package; the body says which and why"),
        (status = 409, description = "A build of the package on that platform has not finished, or another request created or removed the package meanwhile"),
        (status = 413, description = "A file larger than the package's artifact limit"),
    ),
)]
#[post("/package/<pkgbase>/upload?<platform>", data = "<data>")]
pub async fn upload_package(
    services: &State<Services>,
    pkgbase: &str,
    platform: Option<&str>,
    content_type: &ContentType,
    a: Authenticated,
    data: Data<'_>,
) -> Result<Json<UploadedBuild>, ApiError> {
    // Unbounded as a whole: every file is held to its limit as it arrives,
    // the framing to its own, and the count of files to `MAX_FILES`.
    let mut body = data.open(ByteUnit::max_value());
    let arrived = match arrive(services, pkgbase, platform, content_type, &mut body).await {
        Ok(arrived) => arrived,
        Err(e) => {
            discard_rest(&mut body).await;
            return Err(e);
        }
    };
    let request = UploadRequest {
        pkgbase: pkgbase.to_string(),
        platform: arrived.platform,
        uploader: a.username,
    };
    let uploaded = upload::publish_upload(
        &services.db,
        &services.repo,
        &services.activity,
        request,
        arrived.received,
    )
    .await?;
    Ok(Json(uploaded))
}

/// An upload whose files are all in, about to be checked as a whole.
struct Arrived {
    platform: Option<Platform>,
    received: Received,
}

/// Decide whether the upload can be taken at all, then receive its files.
async fn arrive(
    services: &Services,
    pkgbase: &str,
    platform: Option<&str>,
    content_type: &ContentType,
    body: &mut DataStream<'_>,
) -> Result<Arrived, ApiError> {
    let platform = platform
        .map(Platform::from_str)
        .transpose()
        .map_err(|e| err(Status::BadRequest, e))?;
    let boundary = content_type
        .is_form_data()
        .then(|| {
            content_type
                .params()
                .find(|(key, _)| *key == "boundary")
                .map(|(_, value)| value.to_string())
        })
        .flatten()
        .ok_or_else(|| err(Status::BadRequest, "expected a multipart/form-data body"))?;

    let existing = upload::package_for(&services.db, pkgbase).await?;
    // The same backstop a worker's artifacts get, and per package for the
    // same reason; a package that does not exist yet gets the global value.
    let pkg_id = existing.as_ref().map(|pkg| pkg.id);
    let limit = settings::get(&services.db, settings::key::MAX_ARTIFACT_SIZE, pkg_id)
        .await?
        .value
        .0;

    let mut received = Received::new(&services.repo, pkgbase, existing.as_ref(), limit).await?;
    receive(body, boundary, &mut received).await?;
    Ok(Arrived { platform, received })
}

/// Read and drop what is left of a refused upload's body, up to
/// [`DISCARD_LIMIT`]. Only for an authenticated request: one the auth guard
/// refuses never reaches the handler, so nothing reads up to a gigabyte for
/// a client without a token.
///
/// A client is still sending when an upload is refused part-way, or before
/// any of it is read. Closing the connection on it then would have it report
/// a broken pipe rather than the refusal, which it reads once it has sent
/// everything. That includes a client that waits for `100 Continue` before
/// sending: Rocket reads the start of every request's body before routing it,
/// which already tells such a client to go on.
async fn discard_rest(body: &mut DataStream<'_>) {
    let _ = tokio::io::copy(&mut body.take(DISCARD_LIMIT), &mut tokio::io::sink()).await;
}

/// Stream every part of the body into `received`, each within its limit.
async fn receive(
    body: &mut DataStream<'_>,
    boundary: String,
    received: &mut Received,
) -> Result<(), ApiError> {
    let delivered = Arc::new(AtomicU64::new(0));
    let body = Framing {
        inner: body,
        read: 0,
        delivered: Arc::clone(&delivered),
    };
    let stream = tokio_util::io::ReaderStream::new(body);
    let mut multipart = multer::Multipart::new(stream, boundary);
    // The parser's own message for a body that could not be read says only
    // that; the reason, ours included, is its source.
    let malformed = |e: multer::Error| {
        let cause = std::error::Error::source(&e)
            .map(|cause| format!(": {cause}"))
            .unwrap_or_default();
        err(Status::BadRequest, format!("malformed upload: {e}{cause}"))
    };

    while let Some(mut field) = multipart.next_field().await.map_err(malformed)? {
        let Some(filename) = field.file_name().map(str::to_string) else {
            return Err(err(
                Status::BadRequest,
                "every part of an upload is a file, named by its filename",
            ));
        };
        let incoming = received.accept(&filename)?;
        let mut file = tokio::fs::File::create(&incoming.path).await?;
        let mut written: u64 = 0;
        while let Some(chunk) = field.chunk().await.map_err(malformed)? {
            written += chunk.len() as u64;
            delivered.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            if written > incoming.limit {
                let limit = aurcache_common::units::format_size(incoming.limit);
                return Err(err(
                    Status::PayloadTooLarge,
                    if filename.ends_with(".sig") {
                        format!("{filename} is larger than a signature may be ({limit})")
                    } else {
                        format!(
                            "{filename} is larger than {limit} (max_artifact_size); \
                             raise it for the package or globally"
                        )
                    },
                ));
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        if written == 0 {
            return Err(err(Status::BadRequest, format!("{filename} is empty")));
        }
    }
    if received.files().is_empty() {
        return Err(err(Status::BadRequest, "the upload carries no files"));
    }
    Ok(())
}

/// An upload's body, cut off once more of it has gone to multipart framing
/// than [`FRAMING_LIMIT`]: everything read that is not yet `delivered` as
/// file contents.
struct Framing<R> {
    inner: R,
    /// Bytes read from the body so far.
    read: u64,
    /// Bytes of it handed on as file contents so far.
    delivered: Arc<AtomicU64>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Framing<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        ready!(Pin::new(&mut self.inner).poll_read(cx, buf))?;
        self.read += (buf.filled().len() - before) as u64;
        let framing = self
            .read
            .saturating_sub(self.delivered.load(Ordering::Relaxed));
        if framing > FRAMING_LIMIT {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "more than {} of the upload is part headers rather than files",
                    aurcache_common::units::format_size(FRAMING_LIMIT)
                ),
            )));
        }
        Poll::Ready(Ok(()))
    }
}
