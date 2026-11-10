//! Every variable the server reads from its environment, named once.
//!
//! Code reads a variable through its constant here, never through a string of
//! its own, so this list is the whole of the server's environment. A test
//! holds the documentation's environment page to it, and another fails on a
//! read that bypasses it.
//!
//! The settings a page can also set (`JOB_TIMEOUT`, `DATE_FORMAT`, ...) are
//! named by their [`crate::settings::Setting`] instead, which already pairs
//! each one with its variable. Workers have a table of their own, in
//! `aurcache-worker-core`.

/// A variable the server reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Var(pub &'static str);

impl Var {
    /// The variable's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.0
    }

    /// Its value, as [`std::env::var`] reads it.
    pub fn get(self) -> Result<String, std::env::VarError> {
        std::env::var(self.0)
    }

    /// Its value whatever its encoding, as [`std::env::var_os`] reads it: for
    /// a path.
    #[must_use]
    pub fn get_os(self) -> Option<std::ffi::OsString> {
        std::env::var_os(self.0)
    }
}

impl std::fmt::Display for Var {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

// The database.

/// `SQLITE` or `POSTGRESQL`.
pub const DB_TYPE: Var = Var("DB_TYPE");
pub const DB_NAME: Var = Var("DB_NAME");
pub const DB_USER: Var = Var("DB_USER");
pub const DB_PWD: Var = Var("DB_PWD");
pub const DB_HOST: Var = Var("DB_HOST");

// The server itself.

pub const LOG_LEVEL: Var = Var("LOG_LEVEL");
/// `never` turns off coloured logs.
pub const LOG_STYLE: Var = Var("LOG_STYLE");
/// The timezone schedules are read in.
pub const TZ: Var = Var("TZ");
/// Signs the session cookies.
pub const SECRET_KEY: Var = Var("SECRET_KEY");
/// Where workers reach the pacman repository.
pub const AURCACHE_PUBLIC_URL: Var = Var("AURCACHE_PUBLIC_URL");
/// Set by this project's images: the server runs in a container.
pub const AURCACHE_CONTAINER_IMAGE: Var = Var("AURCACHE_CONTAINER_IMAGE");
pub const RETIRED_PACKAGE_GRACE: Var = Var("RETIRED_PACKAGE_GRACE");
pub const ACTIVITY_RETENTION: Var = Var("ACTIVITY_RETENTION");

// Where the server keeps its state.

pub const AURCACHE_CA_DIR: Var = Var("AURCACHE_CA_DIR");
pub const AURCACHE_BUILD_LOG_PATH: Var = Var("AURCACHE_BUILD_LOG_PATH");
pub const AURCACHE_SOURCE_CACHE_PATH: Var = Var("AURCACHE_SOURCE_CACHE_PATH");
pub const AURCACHE_MIRRORLIST_DIR: Var = Var("AURCACHE_MIRRORLIST_DIR");
pub const OFFICIAL_REPO_CACHE_DIR: Var = Var("OFFICIAL_REPO_CACHE_DIR");
pub const OFFICIAL_MIRRORLIST_PATH: Var = Var("OFFICIAL_MIRRORLIST_PATH");
pub const OFFICIAL_MIRRORLIST_SERVERS: Var = Var("OFFICIAL_MIRRORLIST_SERVERS");
/// 0.5.0's name for the repository directory, still read for the mirrorlist.
pub const BUILD_ARTIFACT_DIR: Var = Var("BUILD_ARTIFACT_DIR");
pub const MIRRORLIST_PATH_X86_64: Var = Var("MIRRORLIST_PATH_X86_64");
pub const MIRROR_RANK_SCHEDULE: Var = Var("MIRROR_RANK_SCHEDULE");
/// The servers configured for an architecture, in place of a ranked list.
pub const MIRRORLIST_SERVERS_X86_64: Var = Var("MIRRORLIST_SERVERS_X86_64");
pub const MIRRORLIST_SERVERS_AARCH64: Var = Var("MIRRORLIST_SERVERS_AARCH64");
pub const MIRRORLIST_SERVERS_ARMV7H: Var = Var("MIRRORLIST_SERVERS_ARMV7H");

// The AUR.

pub const AUR_RPC_URL: Var = Var("AUR_RPC_URL");
pub const AUR_GIT_URL: Var = Var("AUR_GIT_URL");

// Parsing PKGBUILDs.

/// Further directories a parse may not read.
pub const AURCACHE_PROTECTED_DIR: Var = Var("AURCACHE_PROTECTED_DIR");
pub const AURCACHE_SANDBOX: Var = Var("AURCACHE_SANDBOX");
pub const AURCACHE_PKGBUILD_BRIDGE: Var = Var("AURCACHE_PKGBUILD_BRIDGE");

// Signing in.

pub const OAUTH_CLIENT_ID: Var = Var("OAUTH_CLIENT_ID");
pub const OAUTH_CLIENT_SECRET: Var = Var("OAUTH_CLIENT_SECRET");
pub const OAUTH_AUTH_URI: Var = Var("OAUTH_AUTH_URI");
pub const OAUTH_TOKEN_URI: Var = Var("OAUTH_TOKEN_URI");
pub const OAUTH_USERINFO_URI: Var = Var("OAUTH_USERINFO_URI");
pub const OAUTH_REDIRECT_URI: Var = Var("OAUTH_REDIRECT_URI");
pub const OAUTH_ALLOWED_USERS: Var = Var("OAUTH_ALLOWED_USERS");

// Workers.

pub const AURCACHE_WORKER_PORT: Var = Var("AURCACHE_WORKER_PORT");
pub const AURCACHE_TLS_SANS: Var = Var("AURCACHE_TLS_SANS");
/// Named in the copy-and-run command the Workers page shows.
pub const AURCACHE_WORKER_IMAGE: Var = Var("AURCACHE_WORKER_IMAGE");
pub const AURCACHE_ENROLLMENT_DIR: Var = Var("AURCACHE_ENROLLMENT_DIR");
pub const AURCACHE_ENROLLMENT_TOKEN: Var = Var("AURCACHE_ENROLLMENT_TOKEN");
pub const AURCACHE_PREAPPROVED_WORKERS: Var = Var("AURCACHE_PREAPPROVED_WORKERS");
/// Set by the hybrid image's entrypoint, for its own worker.
pub const AURCACHE_EMBEDDED_WORKER_SECRET: Var = Var("AURCACHE_EMBEDDED_WORKER_SECRET");
pub const LEASE_TTL: Var = Var("LEASE_TTL");
pub const MAX_ATTEMPTS: Var = Var("MAX_ATTEMPTS");
pub const WORKER_SPILL_DELAY: Var = Var("WORKER_SPILL_DELAY");
pub const WORKER_LIVENESS_TIMEOUT: Var = Var("WORKER_LIVENESS_TIMEOUT");
pub const REAP_INTERVAL: Var = Var("REAP_INTERVAL");
pub const REAP_BACKSTOP_GRACE: Var = Var("REAP_BACKSTOP_GRACE");
pub const WORKER_CERT_VALIDITY_DAYS: Var = Var("WORKER_CERT_VALIDITY_DAYS");

/// Every variable above.
pub const ALL: &[Var] = &[
    DB_TYPE,
    DB_NAME,
    DB_USER,
    DB_PWD,
    DB_HOST,
    LOG_LEVEL,
    LOG_STYLE,
    TZ,
    SECRET_KEY,
    AURCACHE_PUBLIC_URL,
    AURCACHE_CONTAINER_IMAGE,
    RETIRED_PACKAGE_GRACE,
    ACTIVITY_RETENTION,
    AURCACHE_CA_DIR,
    AURCACHE_BUILD_LOG_PATH,
    AURCACHE_SOURCE_CACHE_PATH,
    AURCACHE_MIRRORLIST_DIR,
    OFFICIAL_REPO_CACHE_DIR,
    OFFICIAL_MIRRORLIST_PATH,
    OFFICIAL_MIRRORLIST_SERVERS,
    BUILD_ARTIFACT_DIR,
    MIRRORLIST_PATH_X86_64,
    MIRROR_RANK_SCHEDULE,
    MIRRORLIST_SERVERS_X86_64,
    MIRRORLIST_SERVERS_AARCH64,
    MIRRORLIST_SERVERS_ARMV7H,
    AUR_RPC_URL,
    AUR_GIT_URL,
    AURCACHE_PROTECTED_DIR,
    AURCACHE_SANDBOX,
    AURCACHE_PKGBUILD_BRIDGE,
    OAUTH_CLIENT_ID,
    OAUTH_CLIENT_SECRET,
    OAUTH_AUTH_URI,
    OAUTH_TOKEN_URI,
    OAUTH_USERINFO_URI,
    OAUTH_REDIRECT_URI,
    OAUTH_ALLOWED_USERS,
    AURCACHE_WORKER_PORT,
    AURCACHE_TLS_SANS,
    AURCACHE_WORKER_IMAGE,
    AURCACHE_ENROLLMENT_DIR,
    AURCACHE_ENROLLMENT_TOKEN,
    AURCACHE_PREAPPROVED_WORKERS,
    AURCACHE_EMBEDDED_WORKER_SECRET,
    LEASE_TTL,
    MAX_ATTEMPTS,
    WORKER_SPILL_DELAY,
    WORKER_LIVENESS_TIMEOUT,
    REAP_INTERVAL,
    REAP_BACKSTOP_GRACE,
    WORKER_CERT_VALIDITY_DAYS,
];
