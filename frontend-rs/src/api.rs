//! Where the API lives, and how to reach it.

use aurcache_client::AurCacheClient;

/// The API is on the same origin the page was served from, so the base URL is
/// derived at runtime rather than baked in. The fallback is the dev server.
pub fn api_base() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .map_or_else(
            || "http://localhost:8080/api".to_string(),
            |origin| format!("{origin}/api"),
        )
}

/// One process-wide client: building one per call would throw away connection
/// pooling on every poll tick, and re-read the DOM for a base URL that cannot
/// change without a page load.
static CLIENT: std::sync::LazyLock<Result<AurCacheClient, String>> =
    std::sync::LazyLock::new(|| {
        // Read once: each call reaches into the DOM, and the two reads below
        // must agree with each other anyway.
        let base = api_base();
        let login_url = format!("{base}/login");
        AurCacheClient::new(base, None)
            .map(|client| {
                client.on_unauthorized(move || {
                    if let Some(window) = web_sys::window() {
                        let _ = window.location().assign(&login_url);
                    }
                })
            })
            .map_err(|e| e.to_string())
    });

/// A client against the current origin.
///
/// No token: the session is a cookie the server set during OAuth, and the
/// browser attaches it to same-origin requests on its own.
///
/// The client is wired to send the page to the login flow when the session is
/// refused. A server restart turns every API call into a 401 — the cookie can
/// no longer be decoded — and the only way back in is OAuth again, exactly the
/// trip the server gives an unauthenticated full-page load. A plain page jump
/// rather than a router navigate: `/api/login` is a server route, and a SPA
/// navigation to it would render a frontend error screen instead of the login.
pub fn client() -> Result<AurCacheClient, String> {
    CLIENT.clone()
}

/// Why a page could not load the thing it is about.
///
/// Absence is kept apart from failure: a page for something that is not there
/// redirects somewhere useful, where a failure is shown in place.
#[derive(Clone, PartialEq, Debug)]
pub enum LoadError {
    /// The server answered 404.
    NotFound,
    /// Anything else, as it should be shown.
    Failed(String),
}

impl From<anyhow::Error> for LoadError {
    fn from(e: anyhow::Error) -> Self {
        if aurcache_client::is_not_found(&e) {
            Self::NotFound
        } else {
            Self::Failed(e.to_string())
        }
    }
}

impl From<String> for LoadError {
    fn from(e: String) -> Self {
        Self::Failed(e)
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::Failed(e) => f.write_str(e),
        }
    }
}
