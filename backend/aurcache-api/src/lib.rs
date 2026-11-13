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
mod package;
mod repo;
mod settings;
pub mod spa;
mod stats;
pub mod ui_cache;
mod utils;
mod worker;
