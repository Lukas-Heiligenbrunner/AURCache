//! Failed requests, named in the log.
//!
//! Rocket's own request lines are filtered out (`rocket=warn` in the logger):
//! one per request at `info` would drown everything else. That left the
//! warnings Rocket does print -- a route that answered nothing, a status with
//! no catcher -- with no request attached, so an idle worker's claim, which
//! answered 404 every ten seconds, took reading the code to place. This names
//! every failed response instead: method, path and status.

use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::Status;
use rocket::response::status::Custom;
use rocket::{Request, Response, catch};
use tracing::{info, warn};

/// Logs each response of 400 or above: a client's mistake at `info`, the
/// server's at `warn`.
pub struct FailedRequests;

#[rocket::async_trait]
impl Fairing for FailedRequests {
    fn info(&self) -> Info {
        Info {
            name: "Failed requests",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let status = response.status();
        // The path alone: a query can carry a token.
        let (method, path) = (request.method(), request.uri().path());
        if status.code >= 500 {
            warn!("{method} {path} -> {status}");
        } else if status.code >= 400 {
            info!("{method} {path} -> {status}");
        }
    }
}

/// The answer for a status nothing else answered, in the API's own shape: the
/// status and its reason as text, like [`super::error::ApiError`].
#[catch(default)]
pub fn plain_status(status: Status, _request: &Request<'_>) -> Custom<String> {
    Custom(status, status.reason_lossy().to_string())
}
