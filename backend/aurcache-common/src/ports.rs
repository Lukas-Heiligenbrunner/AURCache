//! The ports the server listens on unless told otherwise.
//!
//! Each is a default only: the server reads `AURCACHE_HTTP_PORT`,
//! `AURCACHE_REPO_PORT` and `AURCACHE_WORKER_PORT` at startup. A client that
//! cannot ask the server falls back to these.

/// Default port for the web UI and the API. Override with `AURCACHE_HTTP_PORT`.
pub const AURCACHE_HTTP_PORT: u16 = 8080;
/// Default port for the pacman repository. Override with `AURCACHE_REPO_PORT`.
pub const AURCACHE_REPO_PORT: u16 = 8081;
/// Default port for the dedicated remote-worker protocol listener (HTTPS + mutual
/// TLS). Kept separate from the human/tooling HTTP API so the UI/API/CLI need no
/// TLS themselves (a reverse proxy can terminate TLS for them independently),
/// while machine-to-machine worker auth is scoped to exactly this surface.
/// Override with `AURCACHE_WORKER_PORT`.
pub const AURCACHE_WORKER_PORT: u16 = 8083;
