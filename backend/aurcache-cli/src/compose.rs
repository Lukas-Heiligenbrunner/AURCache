//! Rendering `docker-compose.yaml` files for the shapes AURCache ships in.
//!
//! Templates rather than a YAML serializer, deliberately. The compose files in
//! the repository are two thirds comment — why the worker needs `privileged`,
//! what the shared `enroll` volume buys, which host name the TLS certificate
//! has to be valid for — and a serializer would strip every one of them and
//! reorder the keys for good measure. The comments are the part a first-time
//! user actually needs, so they are what the template preserves.
//!
//! The output mirrors `compose/docker-compose.yaml` (bundle) and
//! `compose/docker-compose.remote-worker.yaml` (worker). When those change,
//! these should follow.

use aurcache_common::ports::{AURCACHE_HTTP_PORT, AURCACHE_MIRROR_PORT, AURCACHE_WORKER_PORT};
use clap::ValueEnum;
use std::fmt::Write as _;

pub const SERVER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-server:latest";
pub const WORKER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-worker:latest";

/// Static file server for the pacman repository. nginx serves archives with
/// sendfile where Rocket streams them through userspace in 4 KiB chunks.
pub const NGINX_IMAGE: &str = "nginx:alpine";

/// The PostgreSQL major version a generated file runs.
///
/// The image tag and the upgrade step's `TARGET_VERSION` both name it, and a
/// test holds them to this one number. `PGDATA` names it too but is written
/// nowhere: from 18 the image's default is `/var/lib/postgresql/<major>/docker`,
/// the layout the upgrade step expects, and the upgrade step derives its own
/// from `TARGET_VERSION`. So a version bump is two edits, not four.
pub const POSTGRES_MAJOR: u32 = 18;

// Before 18 the image's `PGDATA` is `/var/lib/postgresql/data`, which the
// upgrade step would find nothing in; the file would need `PGDATA` written out.
const _: () = assert!(POSTGRES_MAJOR >= 18);

/// Pinned to the major version *and* the Debian release. `postgres:18` moves
/// to a newer Debian from time to time, and a newer C library sorts text
/// differently under indexes already built; `postgres:latest` can also jump a
/// major version onto a data directory it refuses to start on.
pub const POSTGRES_IMAGE: &str = "postgres:18-trixie";

/// The one-shot container that brings the data directory up to
/// [`POSTGRES_MAJOR`] before the database starts: `pg_upgrade` after a backup
/// when the major version was raised, nothing when it already matches. It is
/// the step TrueNAS's own apps (Nextcloud, Immich) run, and it works under any
/// compose.
pub const POSTGRES_UPGRADE_IMAGE: &str = "ixsystems/postgres-upgrade:1.2.16";

/// The service name of the database, which is also the host name the server
/// connects to.
const DATABASE_SERVICE: &str = "aurcache_database";

/// Role and database name. One name for both, so `psql -U aurcache` lands in
/// the database AURCache uses rather than in `postgres`.
const DATABASE_USER: &str = "aurcache";

/// The uid the postgres images run as, and so the owner the upgrade step
/// insists its data directory has.
const POSTGRES_UID: &str = "999:999";

/// Written into a generated worker file when the operator has not supplied a
/// fingerprint, so the file is obviously unfinished rather than quietly
/// trusting whatever answers.
pub const FINGERPRINT_PLACEHOLDER: &str = "REPLACE_WITH_SERVER_CA_SHA256";

/// The service name the server runs under, which is also the host name workers
/// dial inside the compose network — and therefore a name the server's TLS
/// certificate must be valid for.
const SERVER_SERVICE: &str = "aurcache";

/// Where the worker drops its enrollment request in the bundled setup.
pub const ENROLLMENT_DIR: &str = "/enroll";

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ComposeRole {
    /// The server alone, for a host that gets its workers from elsewhere.
    Backend,
    /// A worker alone, joining a server that already exists.
    Worker,
    /// Server plus one local worker: the turnkey default.
    #[default]
    Bundle,
}

impl ComposeRole {
    /// The conventional file name for this role.
    #[must_use]
    pub const fn default_filename(self) -> &'static str {
        match self {
            Self::Backend => "docker-compose.backend.yaml",
            Self::Worker => "docker-compose.worker.yaml",
            Self::Bundle => "docker-compose.yaml",
        }
    }

    pub const fn has_server(self) -> bool {
        matches!(self, Self::Backend | Self::Bundle)
    }

    pub const fn has_worker(self) -> bool {
        matches!(self, Self::Worker | Self::Bundle)
    }
}

/// Every worker environment variable the CLI knows how to set.
///
/// One type, shared with the `docker run` path, so the variable names and their
/// formatting are decided once. A worker configured through a compose file and
/// the same worker configured through `setup worker` must not disagree about
/// what `WORKER_ARCHES` looks like.
#[derive(Debug, Clone, Default)]
pub struct WorkerEnv {
    pub url: Option<String>,
    pub ca_fingerprint: Option<String>,
    pub enrollment_token: Option<String>,
    pub enrollment_dir: Option<String>,
    pub name: Option<String>,
    pub arches: Vec<String>,
    pub packages: Vec<String>,
    pub concurrency: Option<u32>,
    pub priority: Option<i32>,
    /// What backs the worker's storage pool, and how big it may get.
    pub pool: PoolSetup,
}

/// Where a pool device appears inside the worker's container.
pub const POOL_DEVICE: &str = "/dev/aurcache-pool";
/// Where a pool filesystem is mounted inside the worker's container.
pub const POOL_MOUNT: &str = "/var/lib/aurcache-pool";

/// What backs the worker's storage pool, named as the *host* sees it: the
/// container gets it at [`POOL_DEVICE`] or [`POOL_MOUNT`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PoolBacking {
    /// An image file inside the worker's data volume. Nothing to prepare.
    #[default]
    Image,
    /// A block device -- a zvol, a partition -- formatted the first time.
    Device(String),
    /// An existing btrfs filesystem, mounted there and dedicated to the worker.
    Mount(String),
}

impl PoolBacking {
    /// The ZFS dataset behind a zvol device, when this is one: the part after
    /// `/dev/zvol/`, or a placeholder for a bare `/dev/zdN`, whose path does
    /// not carry the name.
    #[must_use]
    pub fn zvol(&self) -> Option<String> {
        let Self::Device(path) = self else {
            return None;
        };
        if let Some(dataset) = path.strip_prefix("/dev/zvol/") {
            return Some(dataset.to_string());
        }
        let zd = path.strip_prefix("/dev/zd")?;
        (!zd.is_empty() && zd.bytes().all(|b| b.is_ascii_digit()))
            .then(|| "<pool>/<zvol>".to_string())
    }
}

/// How a zvol backing the pool should be tuned, one line each, for the wizard
/// to print and the compose file to carry as comments. `volblocksize` is fixed
/// when the zvol is made, so it is checked rather than set.
#[must_use]
pub fn zvol_tuning(dataset: &str) -> Vec<String> {
    vec![
        "Tuning for a zvol backing the pool (see the docs' \"Storage pool\" page):".to_string(),
        format!("  zfs set compression=off primarycache=metadata logbias=throughput {dataset}"),
        "    btrfs compresses inside the pool, the kernel already caches its data,".to_string(),
        "    and its flushes need not be written twice through the ZIL.".to_string(),
        format!("  zfs get volblocksize {dataset}   # 16K or 32K; fixed at creation"),
        "    To make one: zfs create -s -V <size> -o volblocksize=32K <pool>/<zvol>".to_string(),
        "    (-s: sparse; leave it out to reserve the space). Size it at".to_string(),
        "    WORKER_DISK_MAX plus 5% (at least 2G), or more.".to_string(),
    ]
}

/// The storage pool as `setup` writes it: its backing, whether an image is
/// allocated up front, and its total.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolSetup {
    pub backing: PoolBacking,
    /// Allocate the image in full (`WORKER_DISK_RESERVE`); image only.
    pub reserve: bool,
    /// Everything the worker may store (`WORKER_DISK_MAX`), as written, e.g.
    /// `500G`. `None` keeps the worker's default.
    pub disk_max: Option<String>,
}

impl PoolSetup {
    /// `host:container` for `devices:` or `--device`, for a device backing.
    #[must_use]
    pub fn device_mapping(&self) -> Option<String> {
        match &self.backing {
            PoolBacking::Device(host) => Some(format!("{host}:{POOL_DEVICE}")),
            _ => None,
        }
    }

    /// `host:container` for a bind mount, for a mount backing.
    #[must_use]
    pub fn volume_mapping(&self) -> Option<String> {
        match &self.backing {
            PoolBacking::Mount(host) => Some(format!("{host}:{POOL_MOUNT}")),
            _ => None,
        }
    }
}

impl WorkerEnv {
    /// Only the variables that have a value, in a stable order.
    ///
    /// Unset means absent rather than empty: the worker has its own defaults —
    /// the host name, the host architecture — and an empty string would
    /// override them with nothing instead of leaving them alone.
    #[must_use]
    pub fn variables(&self) -> Vec<EnvVar> {
        let mut pairs = Vec::new();
        let mut push = |key, value: Option<String>| {
            if let Some(value) = value {
                pairs.push(EnvVar { key, value });
            }
        };

        push("AURCACHE_URL", self.url.clone());
        push(
            "AURCACHE_SERVER_CA_FINGERPRINT",
            self.ca_fingerprint.clone(),
        );
        push("AURCACHE_ENROLLMENT_TOKEN", self.enrollment_token.clone());
        push("AURCACHE_ENROLLMENT_DIR", self.enrollment_dir.clone());
        push("WORKER_NAME", self.name.clone());
        push("WORKER_ARCHES", join_list(&self.arches));
        // Policy is written as the machine's starting point, not a pin, so it
        // can be changed from the worker's page afterwards without anyone
        // editing this file. The plain names would lock these values to the
        // machine; the worker this generates is new enough to read either.
        push("WORKER_PACKAGES_DEFAULT", join_list(&self.packages));
        push(
            "WORKER_CONCURRENCY_DEFAULT",
            self.concurrency.map(|v| v.to_string()),
        );
        push(
            "WORKER_PRIORITY_DEFAULT",
            self.priority.map(|v| v.to_string()),
        );
        // What backs the pool is the machine's, so it is pinned; how big it may
        // get is policy, a starting point like the ones above.
        push(
            "WORKER_POOL",
            match &self.pool.backing {
                PoolBacking::Image => None,
                PoolBacking::Device(_) => Some(POOL_DEVICE.to_string()),
                PoolBacking::Mount(_) => Some(POOL_MOUNT.to_string()),
            },
        );
        push(
            "WORKER_DISK_RESERVE",
            (self.pool.reserve && self.pool.backing == PoolBacking::Image)
                .then(|| "true".to_string()),
        );
        push("WORKER_DISK_MAX_DEFAULT", self.pool.disk_max.clone());
        pairs
    }
}

/// One environment variable of a worker's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvVar {
    pub key: &'static str,
    pub value: String,
}

impl std::fmt::Display for EnvVar {
    /// `KEY=value`, as `docker run -e` and a compose file both take it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={}", self.key, self.value)
    }
}

/// A repeated flag becomes one comma-separated variable, which is what the
/// worker parses.
fn join_list(values: &[String]) -> Option<String> {
    (!values.is_empty()).then(|| values.join(","))
}

/// Which database a generated server uses, as chosen on the command line.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum DatabaseKind {
    /// A PostgreSQL service beside the server: what a deployment you keep
    /// should run.
    Postgres,
    /// A single file in the server's volume: nothing more to run, fine for
    /// trying AURCache out.
    Sqlite,
}

/// The server's database, with what rendering it needs.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ComposeDatabase {
    Sqlite,
    /// The password is shared by the server and the database service, and only
    /// has to match between them: the database publishes no port.
    Postgres {
        password: String,
        /// Whether to run [`POSTGRES_UPGRADE_IMAGE`] before the database. It is
        /// a third-party image that rewrites the data directory, so it is only
        /// there when asked for.
        upgrade_step: bool,
    },
}

/// Whether `password` can be written into the file as it is.
///
/// The server puts it into a `postgres://user:password@host` URL without
/// escaping it, and the file into YAML without quoting it, so only characters
/// that mean nothing to either are accepted.
#[must_use]
pub fn is_plain_password(password: &str) -> bool {
    !password.is_empty()
        && password
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
}

#[derive(Debug, Clone)]
pub struct ComposeParams {
    pub role: ComposeRole,
    /// Ignored for a worker file, which has no server.
    pub database: ComposeDatabase,
    pub server_image: String,
    pub worker_image: String,
    pub public_url: String,
    pub log_level: String,
    /// Host names the server's TLS certificate must be valid for. A worker
    /// dialing a name that is not in here fails the handshake.
    pub tls_sans: String,
    pub worker: WorkerEnv,
}

impl Default for ComposeParams {
    fn default() -> Self {
        Self {
            role: ComposeRole::default(),
            database: ComposeDatabase::Sqlite,
            server_image: SERVER_IMAGE.to_string(),
            worker_image: WORKER_IMAGE.to_string(),
            public_url: format!("http://localhost:{AURCACHE_MIRROR_PORT}"),
            log_level: "info".to_string(),
            tls_sans: format!("{SERVER_SERVICE},localhost"),
            worker: WorkerEnv::default(),
        }
    }
}

#[must_use]
pub fn render_compose(params: &ComposeParams) -> String {
    let mut out = String::new();
    out.push_str(&header(params));
    out.push_str("services:\n");

    if params.role.has_server() {
        out.push_str(&server_service(params));
        if let ComposeDatabase::Postgres {
            password,
            upgrade_step,
        } = &params.database
        {
            out.push_str(&database_services(password, *upgrade_step));
        }
        out.push_str(&repo_service());
    }
    if params.role.has_worker() {
        out.push_str(&worker_service(params));
    }

    out.push_str(&volumes(params));
    if params.role.has_server() {
        // A single-service worker file needs no network: it reaches the server
        // over the host's, wherever that server happens to be.
        out.push_str("\nnetworks:\n  aurcache:\n    driver: bridge\n");
    }
    out
}

fn header(params: &ComposeParams) -> String {
    let role = params.role;
    let rule = "# =============================================================================\n";
    let body = match role {
        ComposeRole::Bundle => {
            "# AURCache — bundled single-host setup (the turnkey default)\n\
             # =============================================================================\n\
             # One command, zero edits, zero secrets, zero approval clicks:\n\
             #\n\
             #     docker compose up -d\n\
             #\n\
             # This starts the AURCache server plus a single local build worker. The worker\n\
             # auto-enrolls by dropping its certificate request into the shared `enroll`\n\
             # volume that the server reads — presence in that volume is proof of trust, so\n\
             # no token and no manual approval are needed.\n\
             #\n\
             # To scale local build throughput, raise the worker's concurrency on its page\n\
             # in AURCache (or WORKER_CONCURRENCY_DEFAULT here), or run more workers:\n\
             #     docker compose up -d --scale builder=3\n"
        }
        ComposeRole::Backend => {
            "# AURCache — server only\n\
             # =============================================================================\n\
             # The server with no worker of its own: every builder joins from elsewhere.\n\
             # Nothing will build until at least one does.\n\
             #\n\
             #     docker compose up -d\n\
             #\n\
             # The `enroll` volume is still declared and read: a worker on this same host\n\
             # that mounts it is auto-approved. A worker on other hardware cannot, and is\n\
             # approved by fingerprint, by shared token, or once in the web UI.\n"
        }
        ComposeRole::Worker => {
            "# AURCache — build worker\n\
             # =============================================================================\n\
             # A worker joining an AURCache server that runs elsewhere. Jobs for this\n\
             # machine's native architecture are routed here in preference to emulation.\n\
             #\n\
             # There is no shared `enroll` volume across machines, so trust is explicit:\n\
             #\n\
             #   1. Anti-MITM — pin the server's CA fingerprint, printed in its startup log:\n\
             #          \"Worker CA fingerprint (pin this on workers): <sha256>\"\n\
             #      Without a pin the worker trusts on first use: fine on a trusted\n\
             #      network, not over the public internet.\n\
             #\n\
             #   2. Approving THIS worker — pick one: pre-approve its fingerprint with the\n\
             #      server's AURCACHE_PREAPPROVED_WORKERS, share an AURCACHE_ENROLLMENT_TOKEN,\n\
             #      or approve it once on the Workers page.\n\
             #\n\
             #     docker compose up -d\n"
        }
    };

    let database = match (&params.database, role.has_server()) {
        (ComposeDatabase::Postgres { .. }, true) => {
            "#\n\
             # The server keeps its data in PostgreSQL, in the `aurcache_database` service.\n\
             # Its password was generated for this file and only has to match between the\n\
             # two services; the database publishes no port.\n"
        }
        _ => "",
    };

    format!(
        "{rule}{body}{database}#\n# Generated by `aurcli setup compose`. Edit freely — it is a\n\
         # starting point, not a managed file.\n{rule}\n"
    )
}

fn server_service(params: &ComposeParams) -> String {
    let ComposeParams {
        server_image,
        public_url,
        log_level,
        tls_sans,
        database,
        ..
    } = params;

    let (database_env, database_volume, depends_on) = match database {
        ComposeDatabase::Sqlite => (
            String::new(),
            "\x20     - aurcache_db:/app/db               # SQLite database\n".to_string(),
            String::new(),
        ),
        ComposeDatabase::Postgres { password, .. } => (
            format!(
                "\x20     # PostgreSQL, in the `{DATABASE_SERVICE}` service below.\n\
                 \x20     - DB_TYPE=POSTGRESQL\n\
                 \x20     - DB_HOST={DATABASE_SERVICE}\n\
                 \x20     - DB_USER={DATABASE_USER}\n\
                 \x20     - DB_PWD={password}\n\
                 \x20     - DB_NAME={DATABASE_USER}\n"
            ),
            String::new(),
            // Migrations run at startup, so the server waits for a database
            // that answers rather than failing its first connection.
            format!(
                "\x20   depends_on:\n\
                 \x20     {DATABASE_SERVICE}:\n\
                 \x20       condition: service_healthy\n"
            ),
        ),
    };

    format!(
        "  {SERVER_SERVICE}:\n\
         \x20   # Backend only. The `aurcache` image (without `-server`) is the hybrid\n\
         \x20   # compatibility image, which bundles its own worker — do not use it here.\n\
         \x20   image: {server_image}\n\
         \x20   ports:\n\
         \x20     - \"{AURCACHE_HTTP_PORT}:{AURCACHE_HTTP_PORT}\"   # Web UI + API (plain HTTP; front with a reverse proxy for TLS)\n\
         \x20     # No mirror port here: the `repo` service below publishes nginx on\n\
         \x20     # it instead. Rocket's builtin file handler keeps listening inside\n\
         \x20     # this container for deployments without that service.\n\
         \x20     - \"{AURCACHE_WORKER_PORT}:{AURCACHE_WORKER_PORT}\"   # Remote-worker protocol (HTTPS + mutual TLS)\n\
         \x20   environment:\n\
         \x20     - LOG_LEVEL={log_level}\n\
         \x20     # The timezone cron schedules (the auto-update one) are read in. The\n\
         \x20     # host's by default, through the /etc/localtime mount below; to force\n\
         \x20     # one, set TZ where compose runs (a shell export, or `TZ=Europe/Paris`\n\
         \x20     # in a .env file beside this one). Unset there, it stays unset here.\n\
         \x20     - TZ\n\
         \x20     # The server's TLS certificate must be valid for the hostname each worker\n\
         \x20     # dials. In-compose that is the service name; keep `localhost` so the\n\
         \x20     # UI/API is reachable from the host too.\n\
         \x20     - AURCACHE_TLS_SANS={tls_sans}\n\
         \x20     # Public base URL pacman clients (and workers) use to fetch built\n\
         \x20     # packages. Set this to your host's address or domain for remote clients.\n\
         \x20     - AURCACHE_PUBLIC_URL={public_url}\n\
         \x20     # Auto-approve any worker that can write to the shared `enroll` volume,\n\
         \x20     # mounted read-only here. No secret, no approval click.\n\
         \x20     - AURCACHE_ENROLLMENT_DIR={ENROLLMENT_DIR}\n\
         {database_env}\
         \x20   volumes:\n\
         {database_volume}\
         \x20     - aurcache_repo:/app/repo\n\
         \x20     - aurcache_ca:/app/data/ca      # internal worker CA (persist across restarts)\n\
         \x20     - enroll:{ENROLLMENT_DIR}:ro             # read the workers' enrollment CSRs\n\
         \x20     - /etc/localtime:/etc/localtime:ro   # the host's timezone (see TZ above)\n\
         {depends_on}\
         \x20   networks:\n\
         \x20     - aurcache\n\
         \x20   restart: unless-stopped\n\n"
    )
}

/// The pacman repository, served statically by nginx: the same files Rocket's
/// builtin file handler would serve, at line rate instead of ~200 MB/s. The
/// server remains the only writer (publishing renames into place), so this
/// service mounts the repository volume read-only.
///
/// The nginx config travels inside `command` rather than a mounted file, so a
/// generated file stays one file: paste it into Portainer or Unraid as is,
/// with no sibling files to carry along. (The repository's own
/// `compose/docker-compose.yaml` mounts `docker/repo-nginx.conf` instead -- the same
/// server block, kept as a file because it lives beside it.)
fn repo_service() -> String {
    format!(
        "  repo:\n\
         \x20   # Static file server for the pacman repository.\n\
         \x20   image: {NGINX_IMAGE}\n\
         \x20   ports:\n\
         \x20     - \"{AURCACHE_MIRROR_PORT}:80\"   # Pacman repository (plain HTTP, for `pacman -Sy`)\n\
         \x20   volumes:\n\
         \x20     - aurcache_repo:/app/repo:ro   # read the repository the server publishes\n\
         \x20   command:\n\
         \x20     - sh\n\
         \x20     - -c\n\
         \x20     - |\n\
         \x20       printf '%s' 'server {{\n\
         \x20         listen 80;\n\
         \x20         root /app/repo;\n\
         \x20         sendfile on;\n\
         \x20         tcp_nopush on;\n\
         \x20         server_tokens off;\n\
         \x20         location ~ /\\. {{ return 404; }}\n\
         \x20       }}' > /etc/nginx/conf.d/default.conf\n\
         \x20       exec nginx -g 'daemon off;'\n\
         \x20   depends_on:\n\
         \x20     - aurcache\n\
         \x20   networks:\n\
         \x20     - aurcache\n\
         \x20   restart: unless-stopped\n\n"
    )
}

/// The database, and the step that upgrades its data before it starts when
/// `upgrade_step` asks for one.
///
/// The volume is mounted at `/var/lib/postgresql`, the parent of `PGDATA`
/// rather than `PGDATA` itself: the data lives in `<major>/docker` below it,
/// so an upgrade can build the new version's directory beside the old one.
fn database_services(password: &str, upgrade_step: bool) -> String {
    let env = format!(
        "\x20     - POSTGRES_USER={DATABASE_USER}\n\
         \x20     - POSTGRES_PASSWORD={password}\n\
         \x20     - POSTGRES_DB={DATABASE_USER}\n"
    );

    let (upgrade, version_note, depends_on) = if upgrade_step {
        (
            upgrade_service(&env),
            "\x20   # Change the major version together with TARGET_VERSION above.\n",
            format!(
                "\x20   depends_on:\n\
                 \x20     {DATABASE_SERVICE}_upgrade:\n\
                 \x20       condition: service_completed_successfully\n"
            ),
        )
    } else {
        (
            String::new(),
            // Without the step, a new tag finds no `<new major>/docker` and
            // initialises one: an empty database, with the old one beside it.
            "\x20   # Changing the major version does not upgrade the data: the new version\n\
             \x20   # starts an empty database in <major>/docker, leaving the old one beside\n\
             \x20   # it. Upgrade first (pg_upgrade, or a dump and restore), or regenerate\n\
             \x20   # this file with --postgres-upgrade to have a step that does it.\n",
            String::new(),
        )
    };

    format!(
        "{upgrade}\
         \x20 {DATABASE_SERVICE}:\n\
         {version_note}\
         \x20   image: {POSTGRES_IMAGE}\n\
         \x20   user: \"{POSTGRES_UID}\"\n\
         \x20   environment:\n\
         {env}\
         \x20   volumes:\n\
         \x20     - aurcache_postgres:/var/lib/postgresql\n\
         \x20   # Over TCP rather than the socket: while the image initialises a new\n\
         \x20   # database, a temporary server answers on the socket and then stops.\n\
         \x20   healthcheck:\n\
         \x20     test: [\"CMD\", \"pg_isready\", \"-h\", \"127.0.0.1\", \"-U\", \"{DATABASE_USER}\", \"-d\", \"{DATABASE_USER}\"]\n\
         \x20     interval: 10s\n\
         \x20     timeout: 5s\n\
         \x20     retries: 30\n\
         {depends_on}\
         \x20   networks:\n\
         \x20     - aurcache\n\
         \x20   restart: unless-stopped\n\n"
    )
}

/// The one-shot service that brings the data up to [`POSTGRES_MAJOR`].
fn upgrade_service(env: &str) -> String {
    format!(
        "  {DATABASE_SERVICE}_upgrade:\n\
         \x20   # Runs before the database, then exits. When the data is from an older major\n\
         \x20   # version it backs it up (under backups/ in the volume) and runs pg_upgrade;\n\
         \x20   # otherwise it does nothing. Data an older setup wrote straight into the\n\
         \x20   # volume (PG_VERSION at its root) is first moved into <its version>/docker.\n\
         \x20   #\n\
         \x20   # To move to a new major version, change TARGET_VERSION here and the\n\
         \x20   # database's image tag below together.\n\
         \x20   #\n\
         \x20   # It runs as the postgres user and refuses a volume it does not own. A named\n\
         \x20   # volume already is; for a host directory, `chown -R 999:999` it first.\n\
         \x20   image: {POSTGRES_UPGRADE_IMAGE}\n\
         \x20   # The image's own entrypoint starts a PostgreSQL server; the upgrade is\n\
         \x20   # this. It insists on the PGDATA the database will use, which follows from\n\
         \x20   # TARGET_VERSION (`$$` keeps compose from substituting it first).\n\
         \x20   entrypoint: [\"/bin/bash\", \"-c\", \"export PGDATA=/var/lib/postgresql/$$TARGET_VERSION/docker && exec /upgrade.sh\"]\n\
         \x20   user: \"{POSTGRES_UID}\"\n\
         \x20   environment:\n\
         \x20     - TARGET_VERSION={POSTGRES_MAJOR}\n\
         {env}\
         \x20   volumes:\n\
         \x20     - aurcache_postgres:/var/lib/postgresql\n\
         \x20   network_mode: none\n\
         \x20   restart: \"no\"\n\n"
    )
}

fn worker_service(params: &ComposeParams) -> String {
    let bundled = params.role == ComposeRole::Bundle;
    let mut env = params.worker.clone();

    if bundled {
        // Inside the compose network the server is reachable by service name,
        // and the shared volume is what makes approval automatic.
        env.url
            .get_or_insert_with(|| format!("https://{SERVER_SERVICE}:{AURCACHE_WORKER_PORT}"));
        env.enrollment_dir
            .get_or_insert_with(|| ENROLLMENT_DIR.to_string());
        // A fingerprint pin is meaningless against a server in the same compose
        // project: the CA is created by that very server.
        env.ca_fingerprint = None;
    } else {
        env.url
            .get_or_insert_with(|| format!("https://aurcache.example.com:{AURCACHE_WORKER_PORT}"));
        env.ca_fingerprint
            .get_or_insert_with(|| FINGERPRINT_PLACEHOLDER.to_string());
    }

    let mut out = String::new();
    out.push_str("  builder:\n");
    let _ = writeln!(out, "    image: {}", params.worker_image);
    if bundled {
        out.push_str("    depends_on:\n      - aurcache\n");
    }

    out.push_str("    environment:\n");
    let _ = writeln!(out, "      - RUST_LOG={}", params.log_level);
    for var in env.variables() {
        let _ = writeln!(out, "      - {var}");
    }

    out.push_str("    volumes:\n");
    if bundled {
        out.push_str(
            "      - enroll:/enroll                # writable: drop CSR for auto-enrollment\n",
        );
    }
    match &env.pool.backing {
        PoolBacking::Image => out.push_str(
            "      - worker_data:/var/lib/aurcache-worker   # identity + storage pool (chroots, caches)\n\
             \x20   # The storage pool is one image file in that volume, holding every chroot,\n\
             \x20   # build and cache under one disk quota (WORKER_DISK_MAX, 200G by default).\n\
             \x20   # On ZFS -- TrueNAS -- a zvol is better (`setup compose --pool-device`), or\n\
             \x20   # give the volume a dataset of its own with recordsize=16K,\n\
             \x20   # primarycache=metadata and logbias=throughput. See the docs' \"Storage pool\".\n",
        ),
        PoolBacking::Device(_) | PoolBacking::Mount(_) => {
            out.push_str(
                "      - worker_data:/var/lib/aurcache-worker   # identity (the pool is below)\n",
            );
            if let Some(mapping) = env.pool.volume_mapping() {
                let _ = writeln!(
                    out,
                    "      - {mapping}   # storage pool: a dedicated btrfs filesystem"
                );
            }
        }
    }
    if let Some(mapping) = env.pool.device_mapping() {
        out.push_str(
            "    # The storage pool: every chroot, build and cache, under one disk quota\n\
             \x20   # (WORKER_DISK_MAX). Formatted the first time; a device that already holds\n\
             \x20   # a filesystem the worker did not make is refused, never formatted.\n",
        );
        if let Some(dataset) = env.pool.backing.zvol() {
            for line in zvol_tuning(&dataset) {
                let _ = writeln!(out, "    # {line}");
            }
        }
        out.push_str("    devices:\n");
        let _ = writeln!(out, "      - {mapping}");
    }

    out.push_str(
        "    # devtools builds each package in a systemd-nspawn chroot, which needs the\n\
         \x20   # namespaces and mounts a plain container forbids. `privileged` plus a tmpfs\n\
         \x20   # `/run` is the tightest configuration that works out of the box.\n\
         \x20   privileged: true\n\
         \x20   tmpfs:\n\
         \x20     - /run\n",
    );
    if bundled {
        out.push_str("    networks:\n      - aurcache\n");
    }
    out.push_str("    restart: unless-stopped\n\n");
    out
}

fn volumes(params: &ComposeParams) -> String {
    let role = params.role;
    let mut out = String::from("volumes:\n");
    if role.has_server() {
        out.push_str(match params.database {
            ComposeDatabase::Sqlite => "  aurcache_db:\n",
            ComposeDatabase::Postgres { .. } => "  aurcache_postgres:\n",
        });
        out.push_str("  aurcache_repo:\n  aurcache_ca:\n");
    }
    // The bundle needs it on both sides; the backend declares it so a worker on
    // this host can be added later without editing the server service.
    // (`Bundle` already satisfies `has_server`; no second disjunct.)
    if role.has_server() {
        out.push_str("  enroll:\n");
    }
    if role.has_worker() {
        out.push_str("  worker_data:\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        ComposeDatabase, ComposeParams, ComposeRole, ENROLLMENT_DIR, EnvVar,
        FINGERPRINT_PLACEHOLDER, NGINX_IMAGE, POSTGRES_IMAGE, POSTGRES_MAJOR, PoolBacking,
        PoolSetup, WorkerEnv, is_plain_password, render_compose,
    };
    use aurcache_common::ports::AURCACHE_MIRROR_PORT;
    use yaml_rust2::{Yaml, YamlLoader};

    fn params(role: ComposeRole) -> ComposeParams {
        ComposeParams {
            role,
            ..ComposeParams::default()
        }
    }

    /// Unset must mean absent: the worker defaults to the host's name and
    /// architecture, and an empty value would override those with nothing.
    #[test]
    fn unset_worker_values_produce_no_variable() {
        assert!(WorkerEnv::default().variables().is_empty());
    }

    #[test]
    fn repeated_values_become_one_comma_separated_variable() {
        let env = WorkerEnv {
            arches: vec!["x86_64".to_string(), "aarch64".to_string()],
            ..WorkerEnv::default()
        };
        assert_eq!(
            env.variables(),
            vec![EnvVar {
                key: "WORKER_ARCHES",
                value: "x86_64,aarch64".to_string()
            }]
        );
    }

    fn worker_with_pool(pool: PoolSetup) -> ComposeParams {
        ComposeParams {
            role: ComposeRole::Worker,
            worker: WorkerEnv {
                pool,
                ..WorkerEnv::default()
            },
            ..ComposeParams::default()
        }
    }

    /// Each pool backing is valid YAML, reaches the container where the worker
    /// looks for it, and names it in the environment. The default image needs
    /// neither a device nor a mount.
    #[test]
    fn each_pool_backing_is_wired_into_the_worker() {
        let device = parsed(&worker_with_pool(PoolSetup {
            backing: PoolBacking::Device("/dev/zvol/tank/aurcache".to_string()),
            disk_max: Some("500G".to_string()),
            ..PoolSetup::default()
        }));
        let devices = device["services"]["builder"]["devices"].as_vec().unwrap();
        assert_eq!(
            devices[0].as_str(),
            Some("/dev/zvol/tank/aurcache:/dev/aurcache-pool")
        );
        let env = environment(&device, "builder");
        assert!(
            env.contains(&"WORKER_POOL=/dev/aurcache-pool".to_string()),
            "{env:?}"
        );
        assert!(
            env.contains(&"WORKER_DISK_MAX_DEFAULT=500G".to_string()),
            "{env:?}"
        );

        let mount = parsed(&worker_with_pool(PoolSetup {
            backing: PoolBacking::Mount("/srv/aurcache-pool".to_string()),
            ..PoolSetup::default()
        }));
        let volumes: Vec<&str> = mount["services"]["builder"]["volumes"]
            .as_vec()
            .unwrap()
            .iter()
            .filter_map(Yaml::as_str)
            .collect();
        assert!(
            volumes.contains(&"/srv/aurcache-pool:/var/lib/aurcache-pool"),
            "{volumes:?}"
        );
        assert!(
            environment(&mount, "builder")
                .contains(&"WORKER_POOL=/var/lib/aurcache-pool".to_string())
        );
        assert!(mount["services"]["builder"]["devices"].is_badvalue());

        let image = parsed(&worker_with_pool(PoolSetup {
            reserve: true,
            ..PoolSetup::default()
        }));
        let env = environment(&image, "builder");
        assert!(
            env.contains(&"WORKER_DISK_RESERVE=true".to_string()),
            "{env:?}"
        );
        assert!(
            !env.iter().any(|e| e.starts_with("WORKER_POOL=")),
            "{env:?}"
        );
        assert!(image["services"]["builder"]["devices"].is_badvalue());
    }

    /// A zvol is recognised by its path, and its compose file carries the
    /// tuning for it with the dataset filled in; another device gets none.
    #[test]
    fn a_zvol_pool_comes_with_its_tuning() {
        let device = |path: &str| PoolBacking::Device(path.to_string());
        assert_eq!(
            device("/dev/zvol/tank/aur").zvol().as_deref(),
            Some("tank/aur")
        );
        assert_eq!(device("/dev/zd16").zvol().as_deref(), Some("<pool>/<zvol>"));
        assert_eq!(device("/dev/sdb2").zvol(), None);
        assert_eq!(device("/dev/zd").zvol(), None);
        assert_eq!(PoolBacking::Mount("/dev/zvol/x".to_string()).zvol(), None);

        let render = |path: &str| {
            let params = worker_with_pool(PoolSetup {
                backing: device(path),
                ..PoolSetup::default()
            });
            parsed(&params);
            render_compose(&params)
        };
        let zvol = render("/dev/zvol/tank/aur");
        assert!(
            zvol.contains(
                "zfs set compression=off primarycache=metadata logbias=throughput tank/aur"
            ),
            "{zvol}"
        );
        assert!(!render("/dev/sdb2").contains("zfs set"));
    }

    /// Reserving is an image's; with a device or a mount it means nothing and
    /// is not written.
    #[test]
    fn reserve_is_only_written_for_an_image() {
        let env = WorkerEnv {
            pool: PoolSetup {
                backing: PoolBacking::Device("/dev/sdz".to_string()),
                reserve: true,
                disk_max: None,
            },
            ..WorkerEnv::default()
        };
        assert!(
            !env.variables()
                .iter()
                .any(|var| var.key == "WORKER_DISK_RESERVE")
        );
    }

    /// The file is built by string concatenation, so one bad indent would ship
    /// a compose file docker refuses. Structural `contains` assertions would
    /// not notice; parsing it does.
    #[test]
    fn every_rendered_role_is_valid_yaml() {
        for role in [
            ComposeRole::Backend,
            ComposeRole::Worker,
            ComposeRole::Bundle,
        ] {
            for database in [
                ComposeDatabase::Sqlite,
                postgres(),
                postgres_without_upgrade(),
            ] {
                let rendered = render_compose(&ComposeParams {
                    database: database.clone(),
                    ..params(role)
                });
                YamlLoader::load_from_str(&rendered).unwrap_or_else(|e| {
                    panic!("{role:?} with {database:?} is not valid YAML: {e}\n{rendered}")
                });
            }
        }
    }

    fn postgres() -> ComposeDatabase {
        ComposeDatabase::Postgres {
            password: "s3cret".to_string(),
            upgrade_step: true,
        }
    }

    fn postgres_without_upgrade() -> ComposeDatabase {
        ComposeDatabase::Postgres {
            password: "s3cret".to_string(),
            upgrade_step: false,
        }
    }

    fn parsed(params: &ComposeParams) -> Yaml {
        let rendered = render_compose(params);
        YamlLoader::load_from_str(&rendered)
            .unwrap_or_else(|e| panic!("not valid YAML: {e}\n{rendered}"))
            .remove(0)
    }

    /// A service's `environment` list as `KEY=value` strings.
    fn environment(doc: &Yaml, service: &str) -> Vec<String> {
        doc["services"][service]["environment"]
            .as_vec()
            .unwrap_or_else(|| panic!("{service} has no environment"))
            .iter()
            .map(|v| v.as_str().expect("a KEY=value entry").to_string())
            .collect()
    }

    fn value_of(env: &[String], key: &str) -> Option<String> {
        env.iter()
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(str::to_string))
    }

    /// Cron schedules are read in the server's timezone, so a generated
    /// server follows the host's -- and `TZ` passes through bare, so it is
    /// forced only by setting it where compose runs, never pinned to empty.
    #[test]
    fn a_server_takes_the_hosts_timezone() {
        for role in [ComposeRole::Bundle, ComposeRole::Backend] {
            let doc = parsed(&params(role));
            let env = environment(&doc, "aurcache");
            assert!(env.contains(&"TZ".to_string()), "{role:?}: {env:?}");
            let volumes: Vec<&str> = doc["services"]["aurcache"]["volumes"]
                .as_vec()
                .unwrap()
                .iter()
                .filter_map(Yaml::as_str)
                .collect();
            assert!(
                volumes.contains(&"/etc/localtime:/etc/localtime:ro"),
                "{role:?}: {volumes:?}"
            );
        }
    }

    #[test]
    fn a_sqlite_server_keeps_its_database_in_a_volume_and_runs_no_postgres() {
        let doc = parsed(&params(ComposeRole::Bundle));
        assert!(doc["services"]["aurcache_database"].is_badvalue());
        assert!(!doc["volumes"]["aurcache_db"].is_badvalue());
        let env = environment(&doc, "aurcache");
        assert_eq!(value_of(&env, "DB_TYPE"), None);
    }

    /// The server finds the database by service name, as the role the
    /// database creates, with the same password on both sides.
    #[test]
    fn a_postgres_server_connects_to_the_database_it_is_given() {
        for role in [ComposeRole::Bundle, ComposeRole::Backend] {
            let doc = parsed(&ComposeParams {
                database: postgres(),
                ..params(role)
            });
            let server = environment(&doc, "aurcache");
            let database = environment(&doc, "aurcache_database");
            assert_eq!(value_of(&server, "DB_TYPE").as_deref(), Some("POSTGRESQL"));
            assert_eq!(
                value_of(&server, "DB_HOST").as_deref(),
                Some("aurcache_database")
            );
            assert_eq!(
                value_of(&server, "DB_USER"),
                value_of(&database, "POSTGRES_USER")
            );
            assert_eq!(
                value_of(&server, "DB_NAME"),
                value_of(&database, "POSTGRES_DB")
            );
            assert_eq!(value_of(&server, "DB_PWD").as_deref(), Some("s3cret"));
            assert_eq!(
                value_of(&database, "POSTGRES_PASSWORD").as_deref(),
                Some("s3cret")
            );
            assert!(doc["volumes"]["aurcache_db"].is_badvalue(), "{role:?}");
            assert!(
                !doc["volumes"]["aurcache_postgres"].is_badvalue(),
                "{role:?}"
            );
        }
    }

    /// The image tag and the upgrade's target only work as a pair: an upgrade
    /// to 18 feeding a 17 server is a database that will not start. `PGDATA`
    /// is left to follow from them rather than written out to disagree.
    #[test]
    fn the_postgres_major_version_agrees_everywhere() {
        let doc = parsed(&ComposeParams {
            database: postgres(),
            ..params(ComposeRole::Bundle)
        });
        let upgrade = environment(&doc, "aurcache_database_upgrade");
        let database = environment(&doc, "aurcache_database");

        assert!(POSTGRES_IMAGE.starts_with(&format!("postgres:{POSTGRES_MAJOR}-")));
        assert_eq!(
            doc["services"]["aurcache_database"]["image"].as_str(),
            Some(POSTGRES_IMAGE)
        );
        assert_eq!(
            value_of(&upgrade, "TARGET_VERSION"),
            Some(POSTGRES_MAJOR.to_string())
        );
        // Left to its own entrypoint the image is a server, which never exits
        // and so never lets the database start. `$$` is compose's escape for
        // a `$` the shell is to expand.
        let entrypoint = doc["services"]["aurcache_database_upgrade"]["entrypoint"][2]
            .as_str()
            .expect("an entrypoint script");
        assert_eq!(
            entrypoint,
            "export PGDATA=/var/lib/postgresql/$$TARGET_VERSION/docker && exec /upgrade.sh"
        );
        assert_eq!(value_of(&upgrade, "PGDATA"), None);
        assert_eq!(value_of(&database, "PGDATA"), None);
        // Everything else the upgrade is told is what the database is told.
        let upgrade_rest: Vec<_> = upgrade
            .iter()
            .filter(|kv| !kv.starts_with("TARGET_VERSION="))
            .cloned()
            .collect();
        assert_eq!(upgrade_rest, database);
    }

    /// Start order is the point: upgrade, then a database that answers, then
    /// the server that migrates it.
    #[test]
    fn postgres_services_start_in_order() {
        let doc = parsed(&ComposeParams {
            database: postgres(),
            ..params(ComposeRole::Bundle)
        });
        assert_eq!(
            doc["services"]["aurcache"]["depends_on"]["aurcache_database"]["condition"].as_str(),
            Some("service_healthy")
        );
        assert_eq!(
            doc["services"]["aurcache_database"]["depends_on"]["aurcache_database_upgrade"]
                ["condition"]
                .as_str(),
            Some("service_completed_successfully")
        );
        assert!(
            !doc["services"]["aurcache_database"]["healthcheck"].is_badvalue(),
            "a service_healthy dependency needs a healthcheck"
        );
    }

    /// Declining the step leaves a database that starts on its own, and says
    /// in the file what a version change will then not do.
    #[test]
    fn without_the_upgrade_step_the_database_stands_alone() {
        let params = ComposeParams {
            database: postgres_without_upgrade(),
            ..params(ComposeRole::Bundle)
        };
        let doc = parsed(&params);
        assert!(doc["services"]["aurcache_database_upgrade"].is_badvalue());
        assert!(doc["services"]["aurcache_database"]["depends_on"].is_badvalue());
        assert_eq!(
            doc["services"]["aurcache_database"]["image"].as_str(),
            Some(POSTGRES_IMAGE)
        );
        // The server still waits for it.
        assert_eq!(
            doc["services"]["aurcache"]["depends_on"]["aurcache_database"]["condition"].as_str(),
            Some("service_healthy")
        );
        let rendered = render_compose(&params);
        assert!(rendered.contains("does not upgrade the data"), "{rendered}");
        assert!(!rendered.contains("postgres-upgrade:"), "{rendered}");
    }

    /// A worker file has no server, so it has no database either.
    #[test]
    fn a_worker_file_has_no_database_whatever_is_chosen() {
        let doc = parsed(&ComposeParams {
            database: postgres(),
            ..params(ComposeRole::Worker)
        });
        assert!(doc["services"]["aurcache_database"].is_badvalue());
        assert!(doc["volumes"]["aurcache_postgres"].is_badvalue());
    }

    #[test]
    fn only_passwords_that_need_no_escaping_are_plain() {
        assert!(is_plain_password("Abc-123_x.y~z"));
        for bad in ["", "p@ss", "a/b", "a:b", "with space", "quo\"te", "#hash"] {
            assert!(!is_plain_password(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_backend_role_has_a_server_and_no_builder() {
        let rendered = render_compose(&params(ComposeRole::Backend));
        assert!(rendered.contains("aurcache-server:latest"), "{rendered}");
        assert!(!rendered.contains("builder:"), "{rendered}");
        assert!(!rendered.contains("privileged"), "{rendered}");
    }

    #[test]
    fn the_worker_role_has_a_builder_and_no_server() {
        let rendered = render_compose(&params(ComposeRole::Worker));
        assert!(rendered.contains("builder:"), "{rendered}");
        assert!(!rendered.contains("aurcache-server:latest"), "{rendered}");
        assert!(!rendered.contains("aurcache_db:"), "{rendered}");
    }

    /// A worker file with no fingerprint must look unfinished rather than
    /// silently trusting whatever answers on that address.
    #[test]
    fn a_worker_without_a_pin_gets_a_loud_placeholder() {
        let rendered = render_compose(&params(ComposeRole::Worker));
        assert!(rendered.contains(FINGERPRINT_PLACEHOLDER), "{rendered}");
    }

    #[test]
    fn a_supplied_fingerprint_replaces_the_placeholder() {
        let mut p = params(ComposeRole::Worker);
        p.worker.ca_fingerprint = Some("abc123".to_string());
        let rendered = render_compose(&p);
        assert!(
            rendered.contains("AURCACHE_SERVER_CA_FINGERPRINT=abc123"),
            "{rendered}"
        );
        assert!(!rendered.contains(FINGERPRINT_PLACEHOLDER), "{rendered}");
    }

    /// The whole point of the bundle: the shared volume is what makes approval
    /// automatic, so it has to be read-only on the server and writable on the
    /// worker.
    #[test]
    fn the_bundle_shares_the_enrollment_volume_both_ways() {
        let rendered = render_compose(&params(ComposeRole::Bundle));
        assert!(
            rendered.contains(&format!("enroll:{ENROLLMENT_DIR}:ro")),
            "{rendered}"
        );
        assert!(rendered.contains("- enroll:/enroll  "), "{rendered}");
        assert!(rendered.contains("privileged: true"), "{rendered}");
    }

    /// Pinning a CA against a server in the same compose project is
    /// meaningless — that server issued the CA.
    #[test]
    fn the_bundle_does_not_pin_a_fingerprint() {
        let mut p = params(ComposeRole::Bundle);
        p.worker.ca_fingerprint = Some("abc123".to_string());
        let rendered = render_compose(&p);
        assert!(
            !rendered.contains("AURCACHE_SERVER_CA_FINGERPRINT"),
            "{rendered}"
        );
    }

    /// The server dials by service name inside the network, and its
    /// certificate has to be valid for that name.
    #[test]
    fn the_bundle_worker_dials_the_service_name_covered_by_the_sans() {
        let rendered = render_compose(&params(ComposeRole::Bundle));
        assert!(
            rendered.contains("AURCACHE_URL=https://aurcache:8083"),
            "{rendered}"
        );
        assert!(
            rendered.contains("AURCACHE_TLS_SANS=aurcache,localhost"),
            "{rendered}"
        );
    }

    #[test]
    fn worker_knobs_reach_the_rendered_file() {
        let mut p = params(ComposeRole::Worker);
        p.worker.concurrency = Some(4);
        p.worker.name = Some("arm-box".to_string());
        p.worker.arches = vec!["aarch64".to_string()];
        let rendered = render_compose(&p);
        // A default the server may take over, not a pin.
        assert!(
            rendered.contains("WORKER_CONCURRENCY_DEFAULT=4"),
            "{rendered}"
        );
        assert!(rendered.contains("WORKER_NAME=arm-box"), "{rendered}");
        assert!(rendered.contains("WORKER_ARCHES=aarch64"), "{rendered}");
    }

    /// A service's published ports as `"host:container"` strings.
    fn ports(doc: &Yaml, service: &str) -> Vec<String> {
        doc["services"][service]["ports"]
            .as_vec()
            .unwrap_or_else(|| panic!("{service} publishes no ports"))
            .iter()
            .map(|v| v.as_str().expect("a port mapping").to_string())
            .collect()
    }

    /// The mirror port moved off the server onto nginx: the same host port,
    /// statically served, with the repository mounted read-only and the
    /// server still the only writer.
    #[test]
    fn server_roles_serve_the_repo_statically() {
        for role in [ComposeRole::Bundle, ComposeRole::Backend] {
            let doc = parsed(&params(role));
            assert_eq!(
                doc["services"]["repo"]["image"].as_str(),
                Some(NGINX_IMAGE),
                "{role:?}"
            );
            let repo_ports = ports(&doc, "repo");
            assert!(
                repo_ports
                    .iter()
                    .any(|p| *p == format!("{AURCACHE_MIRROR_PORT}:80")),
                "{role:?}: {repo_ports:?}"
            );
            assert!(
                !ports(&doc, "aurcache")
                    .iter()
                    .any(|p| p.contains(&AURCACHE_MIRROR_PORT.to_string())),
                "{role:?}: the server keeps only API and worker protocol"
            );
            let volumes = doc["services"]["repo"]["volumes"]
                .as_vec()
                .expect("repo mounts the repository");
            assert!(
                volumes
                    .iter()
                    .any(|v| v.as_str() == Some("aurcache_repo:/app/repo:ro")),
                "{role:?}: {volumes:?}"
            );
            let depends_on = doc["services"]["repo"]["depends_on"]
                .as_vec()
                .expect("repo starts after the server");
            assert!(
                depends_on.iter().any(|v| v.as_str() == Some("aurcache")),
                "{role:?}: {depends_on:?}"
            );
        }
    }

    /// A worker file has no repository to serve.
    #[test]
    fn a_worker_file_has_no_repo_service() {
        let doc = parsed(&params(ComposeRole::Worker));
        assert!(doc["services"]["repo"].is_badvalue());
    }

    #[test]
    fn each_role_has_its_own_conventional_filename() {
        assert_eq!(
            ComposeRole::Bundle.default_filename(),
            "docker-compose.yaml"
        );
        assert_ne!(
            ComposeRole::Worker.default_filename(),
            ComposeRole::Backend.default_filename()
        );
    }
}
