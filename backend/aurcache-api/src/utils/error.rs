//! Shared error responses for the HTTP API.
//!
//! Handlers return [`ApiError`] so the status is chosen explicitly at each
//! failure site. In particular a failed database query is a `500` — the request
//! was well-formed and the resource may well exist, the server just could not
//! answer — while `404` is reserved for a row that genuinely is not there.

use rocket::http::Status;
use rocket::response::status::Custom;
use std::fmt::Display;

/// An error response carrying an explicit status and a human-readable message.
pub type ApiError = Custom<String>;

/// Build an [`ApiError`] with the given status.
pub fn err(status: Status, e: impl Display) -> ApiError {
    Custom(status, e.to_string())
}
