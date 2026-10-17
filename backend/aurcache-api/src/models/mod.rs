//! Server-side view of the API shapes.
//!
//! The shapes themselves live in `aurcache-common` so the HTTP client and a
//! browser frontend use the same structs rather than hand-mirrored copies.
//! Only what is genuinely server-only — the request guard — is defined here.

pub mod authenticated;

pub use aurcache_common::api::{aur, builds, operations, package, settings, stats};
