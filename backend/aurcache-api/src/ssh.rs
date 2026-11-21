use aurcache_common::api::ssh::{
    HostKeyPolicy, KeyOrigin, SshInfo, SshKey, TrustedHost, TrustedHosts,
};
use aurcache_utils::services::Services;
use rocket::serde::json::Json;
use rocket::{State, get};
use utoipa::OpenApi;

use crate::auth::Authenticated;

#[derive(OpenApi)]
#[openapi(
    paths(ssh_info),
    components(schemas(SshInfo, SshKey, KeyOrigin, HostKeyPolicy, TrustedHosts, TrustedHost))
)]
pub struct SshApi;

/// The SSH key the server clones package sources with.
///
/// Its public half, to add to a private repository as a deploy key,
/// how the server decides which hosts to trust, and the host keys it trusts,
/// with their fingerprints to check. Never a private key.
#[utoipa::path(
    responses(
            (status = 200, description = "The server's SSH key, host-key policy and trusted host keys", body = SshInfo),
    )
)]
#[get("/ssh")]
pub fn ssh_info(services: &State<Services>, _a: Authenticated) -> Json<SshInfo> {
    Json(services.store.ssh().info())
}
