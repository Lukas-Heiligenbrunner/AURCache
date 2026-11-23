use aurcache_common::api::ssh::SshKey;
use aurcache_utils::services::Services;
use rocket::serde::json::Json;
use rocket::{State, get};
use utoipa::OpenApi;

use crate::auth::Authenticated;

#[derive(OpenApi)]
#[openapi(paths(ssh_key), components(schemas(SshKey)))]
pub struct SshApi;

/// The server's SSH key for cloning package sources.
///
/// Its public half, to add to a private repository as a deploy key. Never a
/// private key.
#[utoipa::path(
    responses(
            (status = 200, description = "The server's SSH key, or why it has none", body = SshKey),
    )
)]
#[get("/ssh")]
pub fn ssh_key(services: &State<Services>, _a: Authenticated) -> Json<SshKey> {
    Json(services.store.ssh().key())
}
