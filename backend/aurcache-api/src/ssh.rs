use aurcache_common::api::ssh::{HostKeyPolicy, KeyOrigin, NamedKey, SshInfo, SshKey};
use aurcache_utils::services::Services;
use rocket::serde::json::Json;
use rocket::{State, get};
use utoipa::OpenApi;

use crate::auth::Authenticated;

#[derive(OpenApi)]
#[openapi(
    paths(ssh_info),
    components(schemas(SshInfo, NamedKey, SshKey, KeyOrigin, HostKeyPolicy))
)]
pub struct SshApi;

/// The SSH key the server clones package sources with.
///
/// Each key's public half, to add to a private repository as a deploy key,
/// and how the server decides which hosts to trust. Never a private key.
#[utoipa::path(
    responses(
            (status = 200, description = "The server's SSH key and host-key policy", body = SshInfo),
    )
)]
#[get("/ssh")]
pub fn ssh_info(services: &State<Services>, _a: Authenticated) -> Json<SshInfo> {
    Json(services.store.ssh().info())
}
