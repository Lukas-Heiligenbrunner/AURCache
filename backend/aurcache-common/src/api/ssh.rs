//! The server's SSH key for cloning package sources.

use serde::{Deserialize, Serialize};

/// The public half of the server's SSH key, so that it can be added to a
/// remote without reading the server's log; or why it has none.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SshKey {
    Ready {
        /// One line, as it goes into `authorized_keys` or a forge's key form.
        public_key: String,
        /// `SHA256:...`, as `ssh-keygen -l` prints it.
        fingerprint: String,
    },
    /// No key could be loaded; SSH sources fail until this is fixed.
    Unavailable { reason: String },
}
