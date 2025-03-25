//! # Arch Mirrors
//! Get and parse the Arch Linux mirrors.
#![warn(invalid_codeblock_attributes)]
pub mod benchmark;
pub mod country;
pub mod mirror;
pub mod protocol;
pub mod status;

pub use crate::mirror::Mirror;
pub use country::Country;
pub use protocol::Protocol;
pub use status::Status;

/// Shorthand for [`Status::get()`](Status::get). This gets the mirror status of all Arch Linux
/// mirrors.
pub async fn get_status() -> reqwest::Result<Status> {
    Status::get_from_default_url().await
}
