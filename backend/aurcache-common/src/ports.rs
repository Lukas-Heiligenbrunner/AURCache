/// Port on which the AURCache HTTP API and file server listen inside the container.
pub const AURCACHE_HTTP_PORT: u16 = 8080;
/// Port on which the AURCache pacman repo / mirror file server listens inside the container.
pub const AURCACHE_MIRROR_PORT: u16 = 8081;
/// Default port for the dedicated remote-worker protocol listener (HTTPS + mutual
/// TLS). Kept separate from the human/tooling HTTP API so the UI/API/CLI need no
/// TLS themselves (a reverse proxy can terminate TLS for them independently),
/// while machine-to-machine worker auth is scoped to exactly this surface.
/// Override with the `AURCACHE_WORKER_PORT` environment variable.
pub const AURCACHE_WORKER_PORT: u16 = 8083;
