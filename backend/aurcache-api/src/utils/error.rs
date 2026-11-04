//! Shared error responses for the HTTP API.
//!
//! Handlers return [`ApiError`] so the status is chosen explicitly at each
//! failure site. In particular a failed database query is a `500` — the request
//! was well-formed and the resource may well exist, the server just could not
//! answer — while `404` is reserved for a row that genuinely is not there.
//! Database, I/O and `anyhow` errors convert into a `500`, so `?` covers that case
//! and only the 4xx answers are spelled out with [`err`].

use rocket::Request;
use rocket::http::Status;
use rocket::response::status::Custom;
use rocket::response::{self, Responder};
use std::fmt::Display;

/// An error response: a status, and a human-readable message as the
/// `text/plain` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: Status,
    pub message: String,
}

/// Build an [`ApiError`] with the given status.
pub fn err(status: Status, e: impl Display) -> ApiError {
    ApiError {
        status,
        message: e.to_string(),
    }
}

impl<'r> Responder<'r, 'static> for ApiError {
    fn respond_to(self, req: &'r Request<'_>) -> response::Result<'static> {
        Custom(self.status, self.message).respond_to(req)
    }
}

impl From<sea_orm::DbErr> for ApiError {
    fn from(e: sea_orm::DbErr) -> Self {
        err(Status::InternalServerError, e)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        err(Status::InternalServerError, e)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        err(Status::InternalServerError, e)
    }
}
