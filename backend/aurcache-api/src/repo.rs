use crate::listen::ListenAddresses;
use aurcache_common::api::repo::{RepoInfo, SigningInfo};
use aurcache_utils::services::Services;
use rocket::http::{ContentType, Status};
use rocket::serde::json::Json;
use rocket::{State, get};
use utoipa::OpenApi;

use crate::auth::Authenticated;

#[derive(OpenApi)]
#[openapi(
    paths(repo_info, repo_key, repo_signing),
    components(schemas(RepoInfo, SigningInfo))
)]
pub struct RepoApi;

/// How this instance publishes its repository.
///
/// Exists so tooling stops guessing. The repository is not always the API host
/// on the default port — a deployment can publish it on another port, behind a
/// reverse proxy, or under a path — and only the server knows which.
#[utoipa::path(
    responses(
            (status = 200, description = "How the pacman repository is published", body = RepoInfo),
    )
)]
#[get("/repo/info")]
pub fn repo_info(
    listen: &State<ListenAddresses>,
    services: &State<Services>,
    _a: Authenticated,
) -> Json<RepoInfo> {
    let port = listen.repo.port();
    Json(RepoInfo {
        name: services.repo.name().to_string(),
        public_url: crate::worker::public_repo_url(port),
        listen_port: Some(port),
        signing: services.repo.signing_info(),
    })
}

/// The public key the repository is signed with, armored, for
/// `pacman-key --add`.
///
/// Unauthenticated, like the repository it lets a client verify: a public key
/// is no secret, and a client sets it up before it has any token. Trust it by
/// the fingerprint `GET /repo/signing` reports, not by where it came from.
#[utoipa::path(
    responses(
            (status = 200, description = "The armored public key", content_type = "application/pgp-keys", body = String),
            (status = 404, description = "This instance does not sign its repository"),
    )
)]
#[get("/repo/key")]
pub fn repo_key(services: &State<Services>) -> Result<(ContentType, String), Status> {
    let key = services.repo.key().ok_or(Status::NotFound)?;
    Ok((
        ContentType::new("application", "pgp-keys"),
        key.public_key().to_string(),
    ))
}

/// Whether the repository is signed, and with which key: `null` when it is
/// not.
///
/// Unauthenticated, like `GET /repo/key`: pacman refuses a signed repository
/// whose key it does not trust, so a client writing its `pacman.conf` needs to
/// know before it has any token. `GET /repo/info` says the same, with the
/// rest of how the repository is published.
#[utoipa::path(
    responses(
            (status = 200, description = "The key the repository is signed with, or null", body = Option<SigningInfo>),
    )
)]
#[get("/repo/signing")]
pub fn repo_signing(services: &State<Services>) -> Json<Option<SigningInfo>> {
    Json(services.repo.signing_info())
}
