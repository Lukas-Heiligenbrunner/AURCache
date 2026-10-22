//! The sidebar shows the running server's version, so this pins the endpoint
//! it reads: `GET /api/version` answers the managed `ServerVersion` in the
//! shape the client and the server share.

mod common;

use common::{TestApi, test_api};
use std::convert::identity;

use rocket::http::Status;

/// The version route reports the running server in the shape the client and
/// the server share — deserialized as the client's own type, not a string
/// match, so the two cannot silently drift apart.
#[rocket::async_test]
async fn version_reports_the_running_server() {
    // The version under test: the route must report this, not any crate's own
    // compile-time version.
    let TestApi { client, .. } = test_api("0.5.0+g8afa04a.dirty", identity).await;
    let response = client.get("/api/version").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    let body = response.into_string().await.expect("body");
    let info: aurcache_client::ServerInfo =
        serde_json::from_str(&body).expect("the version shape the client shares");
    assert_eq!(info.version, "0.5.0+g8afa04a.dirty");
}
