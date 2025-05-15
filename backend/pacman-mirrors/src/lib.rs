//! # Arch Mirrors
//! Get and parse the Arch Linux mirrors.
#![warn(rustdoc::invalid_codeblock_attributes)]
pub mod benchmark;
pub mod country;
pub mod mirror;
pub mod platforms;
pub mod protocol;
pub mod status;

pub use crate::mirror::Mirror;
use crate::platforms::Platform;
pub use country::Country;
pub use protocol::Protocol;
pub use status::Status;

/// Shorthand for [`Status::get()`](Status::get). This gets the mirror status of all Arch Linux
/// mirrors.
pub async fn get_status(platform: Platform) -> anyhow::Result<Status> {
    Status::get_from_default_url(platform).await
}
