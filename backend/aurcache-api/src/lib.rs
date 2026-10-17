#![recursion_limit = "256"]

mod aur;
mod auth;
pub mod backend;
mod build;
pub mod custom_file_server;
mod dump;
#[cfg(feature = "static")]
pub mod embed;
mod health;
pub mod init;
mod log;
// Public so the HTTP client can be type-checked against the exact shapes
// the server serialises, instead of a hand-mirrored copy.
pub mod models;
mod package;
mod repo;
mod settings;
pub mod spa;
mod stats;
mod utils;
mod worker;
mod worker_enroll;
