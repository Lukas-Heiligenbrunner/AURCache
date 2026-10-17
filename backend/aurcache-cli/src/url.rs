//! Host arithmetic on the configured API URL.
//!
//! The CLI is configured with the *API* URL (`http://host:8080/api`), but the
//! questions a new user has next are about other things the same machine
//! serves: the pacman repository on its own port, and the worker protocol on
//! another. Those are all "same host, different port". Pulling the host back
//! out lives in [`aurcache_common::repo::host_from_url`], shared with the
//! worker template; what stays here is CLI-specific interpretation of it.

/// The scheme in `url`, defaulting to `http` when it carries none.
///
/// A scheme-less URL is what someone types when they mean "the obvious thing",
/// and every port this CLI derives is plain HTTP unless the API itself is TLS.
#[must_use]
pub fn scheme_from_url(url: &str) -> &str {
    url.split_once("://").map_or("http", |(scheme, _)| scheme)
}

/// Whether the host names the machine we are on.
///
/// Two callers, for the same underlying reason: a worker beside the server can
/// take the local shortcut, and a `localhost` published URL is the unconfigured
/// default rather than an address any other machine could use.
#[must_use]
pub fn is_loopback(host: &str) -> bool {
    matches!(
        host,
        "localhost" | "127.0.0.1" | "::1" | "[::1]" | "0.0.0.0"
    )
}

#[cfg(test)]
mod tests {
    use super::{is_loopback, scheme_from_url};

    #[test]
    fn loopback_names_are_recognised() {
        for host in ["localhost", "127.0.0.1", "::1", "[::1]", "0.0.0.0"] {
            assert!(is_loopback(host), "{host}");
        }
        assert!(!is_loopback("aurcache.example.com"));
        assert!(!is_loopback("192.168.1.10"));
    }

    #[test]
    fn a_missing_scheme_reads_as_http() {
        assert_eq!(scheme_from_url("https://host/api"), "https");
        assert_eq!(scheme_from_url("http://host/api"), "http");
        assert_eq!(scheme_from_url("host:8080/api"), "http");
    }
}
