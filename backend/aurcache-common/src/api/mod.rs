//! API request and response shapes.
//!
//! Defined here, not in the server, so the HTTP client and a browser frontend
//! use the *same* structs rather than hand-mirrored copies that drift. Several
//! are also read straight out of a database query; those derives are gated
//! behind the `db` feature so this module stays usable without a driver.

pub mod activity;
pub mod aur;
pub mod build_log;
pub mod builds;
pub mod dump;
pub mod events;
pub mod info;
pub mod log;
pub mod operations;
pub mod package;
pub mod repo;
pub mod settings;
pub mod stats;
pub mod waiting;
pub mod worker;
