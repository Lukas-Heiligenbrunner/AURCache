use aurcache_common::api::repo::RepoInfo;
use rocket::get;
use rocket::serde::json::Json;
use utoipa::OpenApi;

use crate::models::authenticated::Authenticated;

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
pub fn repo_info(_a: Authenticated) -> Json<RepoInfo> {
    Json(RepoInfo {
        public_url: crate::worker::public_repo_url(),
    })
}
