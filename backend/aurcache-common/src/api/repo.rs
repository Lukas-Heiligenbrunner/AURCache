//! What a client needs to address the pacman repository.

use serde::{Deserialize, Serialize};

/// How this instance publishes its repository.
///
/// Deliberately the raw `AURCACHE_PUBLIC_URL`, not a finished `Server =` line:
/// the host in it may be the deployment's real public address or may be the
/// `localhost` default, and only the caller can tell whether to keep it. See
/// [`aurcache_common::repo::server_url_for_host`].
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RepoInfo {
    /// The pacman section and database filename prefix. Older servers omit it.
    #[serde(default = "default_name")]
    pub name: String,
    /// Base URL the repository is served on, from `AURCACHE_PUBLIC_URL`, else
    /// `http://localhost:<listen_port>`.
    pub public_url: String,
    /// The port the server's repository listens on, `AURCACHE_REPO_PORT`. A
    /// `public_url` that names no port means this one. Absent from a server
    /// that does not report it, which listens on the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_port: Option<u16>,
}

/// The key a signed repository's packages and databases are signed with, as
/// `GET /api/repo/signing` reports it, without authentication.
///
/// The public key itself is served at `GET /api/repo/key`, without
/// authentication too, as the repository is.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SigningInfo {
    /// The fingerprint of the key clients trust, as `pacman-key --lsign-key`
    /// takes it: 40 uppercase hex digits.
    pub fingerprint: String,
    /// Whether packages published before signing began may still be unsigned.
    /// A client requiring signatures fails on those until this is false.
    pub backfilling: bool,
}

fn default_name() -> String {
    crate::repo::DEFAULT_REPO_NAME.to_string()
}

#[cfg(test)]
mod tests {
    use super::RepoInfo;

    #[test]
    fn older_servers_default_to_repo() {
        let info: RepoInfo =
            serde_json::from_str(r#"{"public_url":"http://localhost:8081"}"#).unwrap();
        assert_eq!(info.name, "repo");
    }
}
