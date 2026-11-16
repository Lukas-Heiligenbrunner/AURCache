use crate::listen::ListenAddresses;
use aurcache_common::api::repo::RepoInfo;
use rocket::serde::json::Json;
use rocket::{State, get};
use utoipa::OpenApi;

use crate::auth::Authenticated;

#[derive(OpenApi)]
#[openapi(paths(repo_info), components(schemas(RepoInfo)))]
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
pub fn repo_info(listen: &State<ListenAddresses>, _a: Authenticated) -> Json<RepoInfo> {
    let port = listen.repo.port();
    Json(RepoInfo {
        public_url: crate::worker::public_repo_url(port),
        listen_port: Some(port),
    })
}
