//! The command line itself: every subcommand, argument and value parser.

use crate::{compose, repo, setup};
use anyhow::Result;
use aurcache_client::{ExistingPackagePolicy, SecretsPolicy};
use aurcache_common::build_state::BuildState;
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum OutputFormat {
    #[default]
    Text,
    Json,
}

#[derive(Parser, Debug)]
#[command(
    name = "aurcli",
    version,
    about = "CLI client for the AURCache API using bearer-token authentication"
)]
pub(crate) struct Cli {
    /// Base URL of the AURCache API.
    #[arg(long, env = "AURCACHE_URL")]
    pub(crate) url: Option<String>,

    /// API token used as Authorization: Bearer <token>.
    #[arg(long, env = "AURCACHE_TOKEN")]
    pub(crate) token: Option<String>,

    /// Output format for typed commands.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub(crate) format: OutputFormat,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum Command {
    /// Check whether the server is healthy, and report its version.
    Health,
    /// Diagnose why builds are not running.
    Doctor,
    /// Stand up a server and/or workers.
    Setup {
        // Boxed: these argument structs are much larger than any other variant,
        // and every parse would otherwise carry that size.
        #[command(subcommand)]
        command: Box<SetupCommand>,
    },
    /// Consume this instance as a pacman repository.
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Print a shell completion script.
    Completions {
        /// Shell to generate completions for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Inspect the authenticated user.
    UserInfo,
    /// Get dashboard statistics.
    Stats,
    /// Get monthly graph datapoints.
    Graph,
    /// Search the AUR through AURCache.
    Search {
        /// Search query.
        query: String,
    },
    /// Manage API tokens.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Manage CLI configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage packages.
    Pkg {
        #[command(subcommand)]
        command: PackagesCommand,
    },
    /// Export the server's authored state to a `.tar.gz`.
    Dump {
        /// Where to write the archive. Defaults to a dated name in the current
        /// directory.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Also export the CA private key, worker certificates and API token
        /// hashes. The file becomes a credential: anyone holding it can mint a
        /// worker this server accepts. Written owner-readable only.
        #[arg(long)]
        include_secrets: bool,
    },
    /// Restore an instance from a dump.
    Restore {
        /// The `.tar.gz` to read.
        archive: PathBuf,
        /// Report what would happen and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// What to do about a package that already exists here.
        #[arg(long, value_enum, default_value_t = OnExisting::Skip)]
        on_existing: OnExisting,
        /// Replace what is here rather than adding to it. Destructive: every
        /// package, setting and worker this instance has is removed first.
        #[arg(long)]
        clear: bool,
        /// Take the dump's CA, worker certificates and token hashes. Replacing
        /// the CA invalidates every certificate this server's workers hold.
        #[arg(long, value_enum, default_value_t = Secrets::Ignore)]
        secrets: Secrets,
    },
    /// Manage builds.
    Builds {
        #[command(subcommand)]
        command: BuildsCommand,
    },
    /// Manage remote build workers.
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },
    /// Call an arbitrary API path.
    Raw(RawArgs),
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum TokenCommand {
    /// Regenerate the currently authenticated user's API token.
    Regenerate,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum WorkerCommand {
    /// List enrolled remote build workers.
    List,
    /// Approve a pending worker so it can build.
    Approve {
        /// Worker id.
        id: i32,
    },
    /// Stop intake: give this worker no new builds -- they go to other
    /// workers -- and let the ones it is running finish. For emptying a
    /// machine before a reboot, an upgrade or retirement.
    ///
    /// Pausing a worker that is already paused changes nothing, so running
    /// it again with `--wait` is how to wait for a worker paused earlier.
    Pause {
        /// Worker id.
        id: i32,
        /// Then wait until the worker is running no builds, printing what it
        /// is still running as that changes. A build that has handed its
        /// packages over (`publishing`) no longer counts: the worker is done
        /// with it.
        #[arg(long)]
        wait: bool,
        /// Give up waiting after this many seconds, and exit non-zero. Waits
        /// as long as it takes when unset: some builds run for hours.
        #[arg(long = "wait-timeout", requires = "wait")]
        wait_timeout: Option<u64>,
    },
    /// Resume intake on a worker whose intake was stopped.
    Resume {
        /// Worker id.
        id: i32,
    },
    /// Revoke a worker, immediately refusing its certificate.
    Revoke {
        /// Worker id.
        id: i32,
    },
    /// Show a worker's settings -- what it runs and where each value came
    /// from -- or change them with `--set` and `--reset`.
    ///
    /// Every change in one call is saved together, and the worker picks it up
    /// on its next heartbeat. A setting pinned in the worker's own environment
    /// keeps that value until the variable is renamed to `<VAR>_DEFAULT`.
    Config {
        /// Worker id.
        id: i32,
        /// Set a value, as `key=value`. Repeatable.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Remove the value set here, so the worker falls back to its own.
        /// Repeatable.
        #[arg(long = "reset", value_name = "KEY")]
        reset: Vec<String>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum SetupCommand {
    /// Write a docker-compose file to run elsewhere — TrueNAS, Portainer,
    /// Unraid, or `docker compose up` on any host.
    Compose(ComposeArgs),
    /// Run the AURCache server on this machine with `docker run`.
    Server(SetupServerArgs),
    /// Run a build worker on this machine with `docker run`.
    Worker(SetupWorkerArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ComposeArgs {
    /// Which services the file should contain.
    #[arg(long, value_enum, default_value_t = compose::ComposeRole::Bundle)]
    pub(crate) role: compose::ComposeRole,

    /// The server's database. Asked for when omitted, and required when there
    /// is no terminal to ask on. A worker file has no database.
    #[arg(long, value_enum)]
    pub(crate) database: Option<compose::DatabaseKind>,

    /// Password for the PostgreSQL database. Generated when omitted; only
    /// letters, digits and `-_.~`, since it goes into a connection URL as is.
    #[arg(long)]
    pub(crate) db_password: Option<String>,

    /// Add a step that upgrades PostgreSQL's data when its major version is
    /// raised (the `ixsystems/postgres-upgrade` image TrueNAS apps use). Asked
    /// for when neither this nor `--no-postgres-upgrade` is given, and left
    /// out when there is no terminal to ask on.
    #[arg(long, conflicts_with = "no_postgres_upgrade")]
    pub(crate) postgres_upgrade: bool,

    /// Leave the PostgreSQL upgrade step out without being asked.
    #[arg(long)]
    pub(crate) no_postgres_upgrade: bool,

    /// Where to write it. `-` writes to stdout.
    #[arg(long, short = 'o')]
    pub(crate) output: Option<String>,

    /// Overwrite an existing file.
    #[arg(long)]
    pub(crate) force: bool,

    /// Base URL pacman clients use to fetch built packages.
    #[arg(long)]
    pub(crate) public_url: Option<String>,

    /// Host names the server's TLS certificate must be valid for.
    #[arg(long)]
    pub(crate) tls_sans: Option<String>,

    #[command(flatten)]
    pub(crate) worker: WorkerKnobs,

    #[command(flatten)]
    pub(crate) common: SetupCommon,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct SetupServerArgs {
    /// Print the `docker run` command instead of running it.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Base URL pacman clients use to fetch built packages.
    #[arg(long)]
    pub(crate) public_url: Option<String>,

    /// Host names the server's TLS certificate must be valid for.
    #[arg(long)]
    pub(crate) tls_sans: Option<String>,

    /// Extra arguments passed to `docker run`, before the image.
    #[arg(last = true)]
    pub(crate) docker_args: Vec<String>,

    #[command(flatten)]
    pub(crate) common: SetupCommon,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct SetupWorkerArgs {
    /// Print the `docker run` command instead of running it.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Worker-protocol URL of the server to join, e.g.
    /// `https://aurcache.example.com:8083`. Defaults to a server on this
    /// machine, which is also what makes the worker auto-approved.
    #[arg(long)]
    pub(crate) server_url: Option<String>,

    /// SHA-256 of the server's CA, from its startup log line
    /// "Worker CA fingerprint (pin this on workers)". Strongly recommended for
    /// a server reached over anything but a trusted network.
    #[arg(long)]
    pub(crate) ca_fingerprint: Option<String>,

    /// Container name. Give each worker on a host its own.
    #[arg(long, default_value = setup::WORKER_CONTAINER)]
    pub(crate) container_name: String,

    /// Volume holding the worker's identity and its storage pool: the base
    /// chroot, the builds and the caches. Each worker on a host needs its own,
    /// or they fight over one identity.
    #[arg(long)]
    pub(crate) data_volume: Option<String>,

    /// Extra arguments passed to `docker run`, before the image.
    #[arg(last = true)]
    pub(crate) docker_args: Vec<String>,

    #[command(flatten)]
    pub(crate) worker: WorkerKnobs,

    #[command(flatten)]
    pub(crate) common: SetupCommon,
}

/// Worker settings shared by `setup compose` and `setup worker`.
#[derive(Args, Debug, Clone)]
pub(crate) struct WorkerKnobs {
    /// Name shown in the Workers UI. Defaults to the container's hostname.
    #[arg(long = "worker-name")]
    pub(crate) worker_name: Option<String>,

    /// Architecture built natively. Repeat for several. Defaults to the host's.
    #[arg(long = "arch")]
    pub(crate) arches: Vec<String>,

    /// Reserve a pkgbase to this worker. Repeat for several.
    #[arg(long = "package")]
    pub(crate) packages: Vec<String>,

    /// Packages built in parallel.
    #[arg(long)]
    pub(crate) concurrency: Option<u32>,

    /// Scheduling preference; higher wins.
    #[arg(long)]
    pub(crate) priority: Option<i32>,

    /// Shared secret matching the server's `AURCACHE_ENROLLMENT_TOKEN`.
    #[arg(long)]
    pub(crate) enrollment_token: Option<String>,

    /// Back the worker's storage pool with this block device on the host: a
    /// zvol or a partition, formatted the first time. One that already holds a
    /// filesystem the worker did not make is refused. The best choice on ZFS.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["pool_mount", "pool_image"])]
    pub(crate) pool_device: Option<String>,

    /// Back it with an existing btrfs filesystem mounted at PATH on the host.
    /// It must be a whole filesystem, dedicated to the worker: quotas apply to
    /// all of it.
    #[arg(long, value_name = "PATH", conflicts_with = "pool_image")]
    pub(crate) pool_mount: Option<String>,

    /// Back it with an image file in the worker's data volume -- the default,
    /// which needs nothing prepared on the host.
    #[arg(long)]
    pub(crate) pool_image: bool,

    /// Allocate the image in full up front, so its space is the worker's.
    /// Image only; on ZFS it reserves nothing.
    #[arg(long, conflicts_with_all = ["pool_device", "pool_mount"])]
    pub(crate) disk_reserve: bool,

    /// Everything the worker may store -- base chroot, builds, caches -- e.g.
    /// `500G` (`WORKER_DISK_MAX`, 200G by default). With a device or a mount,
    /// leave about 5% of it, and at least 2G, free.
    #[arg(long, value_name = "SIZE", value_parser = parse_size_value)]
    pub(crate) disk_max: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct SetupCommon {
    /// Server image to run.
    #[arg(long)]
    pub(crate) server_image: Option<String>,

    /// Worker image to run.
    #[arg(long)]
    pub(crate) worker_image: Option<String>,

    /// Log level for the containers.
    #[arg(long, default_value = "info")]
    pub(crate) log_level: String,
}

/// A size as the worker reads it (`500G`, `1T`, bytes), checked here so a typo
/// fails now rather than as a default on the worker.
pub(crate) fn parse_size_value(raw: &str) -> Result<String, String> {
    aurcache_common::units::parse_size(raw)
        .filter(|&bytes| bytes > 0)
        .map(|_| raw.trim().to_string())
        .ok_or_else(|| format!("{raw:?} is not a size (e.g. 500G, 1T)"))
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum RepoCommand {
    /// Print the `pacman.conf` stanza for this instance.
    Config(RepoConfigArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct RepoConfigArgs {
    /// Add the stanza to pacman.conf instead of printing it. Asks for sudo only
    /// if the file is not already writable, and does nothing if the repository
    /// is already configured.
    #[arg(long)]
    pub(crate) install: bool,

    /// The pacman configuration to write to.
    #[arg(long, default_value = repo::PACMAN_CONF)]
    pub(crate) pacman_conf: PathBuf,

    /// Do not ask the server how it publishes the repository; derive the
    /// address from the configured URL alone.
    #[arg(long)]
    pub(crate) offline: bool,

    /// Port the pacman repository is published on.
    #[arg(long)]
    pub(crate) port: Option<u16>,

    /// Section name, which must match the repository's database files.
    #[arg(long)]
    pub(crate) name: Option<String>,

    /// `SigLevel` to write. AURCache does not sign packages, so a stricter
    /// level than the default refuses everything it serves.
    #[arg(long)]
    pub(crate) siglevel: Option<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigCommand {
    /// Show the saved config location and stored values.
    Show,
    /// Save the AURCache URL to the config file.
    SetUrl {
        /// URL to save. If omitted, it will be requested interactively.
        url: Option<String>,
    },
    /// Save the AURCache API token to the config file.
    SetToken {
        /// Token to save. If omitted, it will be requested interactively.
        token: Option<String>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum PackagesCommand {
    /// List directly requested packages.
    List(ListPackagesArgs),
    /// Get one package.
    Get {
        /// Package name (pkgbase).
        pkgbase: String,
    },
    /// Add a package. Each entry is treated as a git repository URL if it
    /// looks like one (contains `@` or a URL scheme like `https://`),
    /// otherwise as an AUR package name.
    Add(AddPackageArgs),
    /// Trigger an update check for a package.
    Update(UpdatePackageArgs),
    /// Partially update package metadata.
    Patch(PatchPackageArgs),
    /// Inspect and repoint a package's dependencies.
    Dep {
        #[command(subcommand)]
        command: DependencyCommand,
    },
    /// Remove the direct-request flag from one or more packages.
    #[command(visible_alias = "delete")]
    Rm {
        /// Package names (pkgbase). Accepts several, so the names printed by
        /// `pkg list -q` can be passed straight through.
        #[arg(required = true)]
        pkgbases: Vec<String>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DependencyCommand {
    /// List what could take over a dependency.
    Options {
        /// Package whose dependency this is.
        pkgbase: String,
        /// The dependency to replace, by pkgbase.
        dependency: String,
    },
    /// Point a dependency at another package.
    Replace {
        /// Package whose dependency this is.
        pkgbase: String,
        /// The dependency to replace, by pkgbase.
        dependency: String,
        /// What to depend on instead. Added from the AUR if it is not tracked
        /// yet.
        replacement: String,
    },
    /// Drop a dependency the official repositories now publish.
    ///
    /// Refused for anything they do not, since resolution would put the edge
    /// straight back at the next update.
    Drop {
        /// Package whose dependency this is.
        pkgbase: String,
        /// The dependency to drop, by pkgbase.
        dependency: String,
    },
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ListPackagesArgs {
    /// Maximum number of packages to return.
    #[arg(long)]
    pub(crate) limit: Option<u64>,

    /// Page offset used together with --limit.
    #[arg(long)]
    pub(crate) page: Option<u64>,

    /// Include packages that are only present as dependencies.
    #[arg(long)]
    pub(crate) all: bool,

    /// Print only the package names, one per line, so the list can be fed to
    /// another command such as `pkg rm`.
    #[arg(long, short = 'q')]
    pub(crate) quiet: bool,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct AddPackageArgs {
    /// AUR package names and/or git repository URLs. Each entry is treated
    /// as a git URL if it looks like one (contains `@` or a URL scheme like
    /// `https://`), otherwise as an AUR package name.
    #[arg(required_unless_present = "from_installed")]
    pub(crate) packages: Vec<String>,

    /// Also add every AUR package already installed on this machine, as
    /// reported by `pacman -Qm`.
    #[arg(long)]
    pub(crate) from_installed: bool,

    /// Skip the confirmation prompt. Required with --from-installed when not
    /// running on a terminal, because that list is not one to submit unseen.
    #[arg(long, short = 'y')]
    pub(crate) yes: bool,

    /// Git ref to checkout. Required if any entry is a git URL.
    #[arg(long = "ref")]
    pub(crate) git_ref: Option<String>,

    /// Subfolder containing the PKGBUILD, for git URL entries.
    #[arg(long, default_value = "")]
    pub(crate) subfolder: String,

    /// Target platform. Repeat for multiple platforms.
    #[arg(long = "platform")]
    pub(crate) platforms: Vec<String>,

    /// Build flag. Repeat for multiple flags.
    #[arg(long = "build-flag")]
    pub(crate) build_flags: Vec<String>,

    /// Patch a source file before adding: either `SOURCE_PATH=LOCAL_FILE`
    /// (e.g. `--patch PKGBUILD=./fixed-PKGBUILD`) or just `LOCAL_FILE`, in
    /// which case the file's own base name is used as the source path (e.g.
    /// `--patch ./PKGBUILD` patches `PKGBUILD`). Repeat for multiple files.
    /// Only valid when adding a single package.
    #[arg(long = "patch", value_parser = parse_patch_arg)]
    pub(crate) patches: Vec<PatchArg>,

    #[command(flatten)]
    pub(crate) wait: WaitOpts,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct UpdatePackageArgs {
    /// Package name (pkgbase).
    pub(crate) pkgbase: String,

    /// Force the update even when the version did not change.
    #[arg(long)]
    pub(crate) force: bool,

    #[command(flatten)]
    pub(crate) wait: WaitOpts,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct PatchPackageArgs {
    /// Package name (pkgbase).
    pub(crate) pkgbase: String,

    /// Platform selection. Repeat to replace with multiple values.
    #[arg(long = "platform")]
    pub(crate) platforms: Vec<String>,

    /// Build flag selection. Repeat to replace with multiple values.
    #[arg(long = "build-flag")]
    pub(crate) build_flags: Vec<String>,

    /// Patch a source file: either `SOURCE_PATH=LOCAL_FILE` (e.g.
    /// `--patch PKGBUILD=./fixed-PKGBUILD`) or just `LOCAL_FILE`, in which
    /// case the file's own base name is used as the source path
    /// (e.g. `--patch ./PKGBUILD` patches `PKGBUILD`). Repeat for multiple
    /// files.
    #[arg(long = "patch", value_parser = parse_patch_arg)]
    pub(crate) patches: Vec<PatchArg>,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum BuildsCommand {
    /// List builds.
    List(ListBuildsArgs),
    /// Get one build.
    Get {
        /// Build reference, e.g. `hello/3`.
        #[arg(value_parser = BuildRef::parse_id)]
        build: BuildRef,
    },
    /// Fetch build output.
    Output(BuildOutputArgs),
    /// Retry a build.
    Retry {
        /// Build reference, e.g. `hello/3`.
        #[arg(value_parser = BuildRef::parse_id)]
        build: BuildRef,
        #[command(flatten)]
        wait: WaitOpts,
    },
    /// Cancel a build.
    Cancel {
        /// Build reference, e.g. `hello/3`.
        #[arg(value_parser = BuildRef::parse_id)]
        build: BuildRef,
    },
    /// Delete a build.
    Delete {
        /// Build reference, e.g. `hello/3`.
        #[arg(value_parser = BuildRef::parse_id)]
        build: BuildRef,
    },
    /// Follow builds until they finish, reporting progress.
    Watch(WatchArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct WatchArgs {
    /// Only follow builds of this package.
    #[arg(long)]
    pub(crate) package: Option<String>,
    /// Give up after this many seconds.
    #[arg(long, default_value_t = 900)]
    pub(crate) timeout: u64,
    /// Fail if nothing changes for this long while nothing is building.
    ///
    /// A queue that is stuck cannot recover on its own, so waiting out the full
    /// timeout only delays the diagnosis. A running build is never treated as a
    /// stall, however slow it is.
    #[arg(long = "stall-after", default_value_t = 120)]
    pub(crate) stall_after: u64,
    /// Seconds between progress lines while work is in flight.
    #[arg(long, default_value_t = 60)]
    pub(crate) heartbeat: u64,
    /// Follow this build even if it has already finished, e.g. `hello/3`.
    /// Repeat for several.
    ///
    /// Without it, a build that finished before this command's first listing is
    /// treated as history and does not affect the exit code — there is no way
    /// to tell it apart from any other old build. A caller that knows what it
    /// triggered says so here; `--wait` on the triggering command does it for
    /// you, and cannot miss the builds the trigger created.
    #[arg(long = "build")]
    #[arg(value_parser = BuildRef::parse_id)]
    pub(crate) builds: Vec<BuildRef>,
}

/// Wait for what a trigger queues, on the command that triggers it.
///
/// This exists because `trigger; watch` is two processes and the gap between
/// them is unobservable: a build can be created, run and fail inside it, and
/// the watcher that starts afterwards cannot distinguish that from a failure
/// last week. One process can — it lists the builds *before* it fires — so the
/// wait is offered where the trigger is rather than as advice to watch quickly.
#[derive(Args, Debug, Clone, Default)]
pub(crate) struct WaitOpts {
    /// Wait for the builds this queues, and exit non-zero if any of them fails.
    #[arg(long)]
    pub(crate) wait: bool,
    /// Give up after this many seconds.
    #[arg(long = "wait-timeout", default_value_t = 900)]
    pub(crate) wait_timeout: u64,
    /// Fail if nothing changes for this long while nothing is building.
    #[arg(long = "wait-stall-after", default_value_t = 120)]
    pub(crate) wait_stall_after: u64,
}

impl WaitOpts {
    /// The watch this wait performs. `package` is left unset: a trigger fans
    /// out into dependency builds under other names, and filtering to the
    /// package named on the command line would hide exactly the builds the
    /// trigger is waiting for.
    pub(crate) fn as_watch_args(&self) -> WatchArgs {
        WatchArgs {
            package: None,
            timeout: self.wait_timeout,
            stall_after: self.wait_stall_after,
            heartbeat: 60,
            builds: Vec::new(),
        }
    }
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ListBuildsArgs {
    /// Optional package name (pkgbase) to filter by.
    #[arg(long = "package")]
    pub(crate) pkgbase: Option<String>,

    /// Only builds this worker ran or is running, by id or name.
    #[arg(long)]
    pub(crate) worker: Option<String>,

    /// Only builds in these states, comma-separated or repeated: active,
    /// successful, failed, enqueued, waiting-for-deps, publishing.
    #[arg(long, value_delimiter = ',', value_parser = parse_build_state)]
    pub(crate) status: Vec<BuildState>,

    /// Maximum number of builds to return.
    #[arg(long)]
    pub(crate) limit: Option<u64>,

    /// Page offset used together with --limit.
    #[arg(long)]
    pub(crate) page: Option<u64>,
}

#[derive(Args, Debug, Clone)]
/// Fetch one build's log output, bounded to server-sized pages.
pub(crate) struct BuildOutputArgs {
    /// Build reference, e.g. `hello/3`.
    #[arg(value_parser = BuildRef::parse_id)]
    pub(crate) build: BuildRef,

    /// Skip this many bytes of log before printing.
    ///
    /// Bytes rather than lines: the server seeks to the offset, so the cost
    /// is proportional to what is read rather than to the whole log. An
    /// offset in the middle of a multi-byte character drops that character.
    #[arg(long = "offset")]
    pub(crate) offset: Option<u64>,

    /// Maximum number of bytes of log to print.
    ///
    /// The log is fetched in bounded pages whatever the value, so the
    /// server never holds more than one page; this only caps what reaches
    /// stdout (or the JSON `output` string).
    #[arg(long = "limit")]
    pub(crate) limit: Option<u64>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct RawArgs {
    /// HTTP method to use, for example GET or POST.
    pub(crate) method: String,

    /// API path such as /packages/list or a full URL.
    pub(crate) path: String,

    /// Query parameter in KEY=VALUE form. Repeat to pass multiple pairs.
    #[arg(long = "query", value_parser = parse_key_val)]
    pub(crate) query: Vec<(String, String)>,

    /// JSON body to send.
    #[arg(long)]
    pub(crate) body: Option<String>,
}

/// What an import should do about the dump's secrets.
#[derive(Copy, Clone, PartialEq, Eq, Debug, ValueEnum)]
pub(crate) enum Secrets {
    /// Leave this server's CA and tokens alone.
    Ignore,
    /// Take the dump's, replacing what is here.
    Copy,
}

impl From<Secrets> for SecretsPolicy {
    fn from(value: Secrets) -> Self {
        match value {
            Secrets::Ignore => Self::Ignore,
            Secrets::Copy => Self::Copy,
        }
    }
}

/// What an import should do about a package that is already here.
#[derive(Copy, Clone, PartialEq, Eq, Debug, ValueEnum)]
pub(crate) enum OnExisting {
    /// Leave it alone.
    Skip,
    /// Replace its configuration with the dump's.
    Overwrite,
    /// Leave it alone, but take the dump's patch if it has none. Refuses the
    /// import when both sides carry one.
    MergePatches,
}

impl From<OnExisting> for ExistingPackagePolicy {
    fn from(value: OnExisting) -> Self {
        match value {
            OnExisting::Skip => Self::Skip,
            OnExisting::Overwrite => Self::Overwrite,
            OnExisting::MergePatches => Self::MergePatches,
        }
    }
}

/// One `--patch`: which source file to replace, and the local file to replace
/// it with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PatchArg {
    pub(crate) source_path: String,
    pub(crate) local_file: String,
}

/// Parses a single `--patch` argument, accepting either
/// `SOURCE_PATH=LOCAL_FILE` or just `LOCAL_FILE` (in which case the file's
/// own base name is used as the source path).
pub(crate) fn parse_patch_arg(s: &str) -> Result<PatchArg, String> {
    match s.split_once('=') {
        Some((path, file)) => {
            if path.is_empty() {
                return Err("invalid --patch value: SOURCE_PATH must not be empty".to_string());
            }
            Ok(PatchArg {
                source_path: path.to_string(),
                local_file: file.to_string(),
            })
        }
        None => {
            let path = Path::new(s)
                .file_name()
                .ok_or_else(|| format!("invalid --patch value `{s}`"))?
                .to_string_lossy()
                .into_owned();
            Ok(PatchArg {
                source_path: path,
                local_file: s.to_string(),
            })
        }
    }
}

pub(crate) fn parse_build_state(key: &str) -> Result<BuildState, String> {
    let mut states = BuildState::parse_keys(key)?;
    match (states.pop(), states.is_empty()) {
        (Some(state), true) => Ok(state),
        _ => Err(format!("{key:?} is not one build state")),
    }
}

pub(crate) fn parse_key_val(input: &str) -> Result<(String, String), String> {
    let (key, value) = input
        .split_once('=')
        .ok_or_else(|| "expected KEY=VALUE".to_string())?;
    if key.is_empty() {
        return Err("query key cannot be empty".to_string());
    }
    Ok((key.to_string(), value.to_string()))
}

/// A build named the way the rest of the system names it: `<pkgbase>/<number>`.
///
/// Parsed from one argument rather than two so `builds show hello/3` reads the
/// way the UI and the URLs write it. The separator is `/` because a pkgbase
/// cannot contain one — and because `#` would have to be quoted in most
/// shells, and `:` already means an epoch in a version.
pub(crate) use aurcache_client::BuildRef;

#[cfg(test)]
mod build_ref_tests {
    use super::BuildRef;

    #[test]
    fn a_reference_is_a_package_and_a_number() {
        let parsed = BuildRef::parse_id("hello/3").expect("parses");
        assert_eq!(parsed.pkgbase, "hello");
        assert_eq!(parsed.number, 3);
    }

    /// Package names contain the characters the AUR allows, none of which is a
    /// slash — so the last slash is always the separator.
    #[test]
    fn awkward_package_names_survive() {
        for (input, pkgbase) in [
            ("aewm++/1", "aewm++"),
            ("2048.c/12", "2048.c"),
            ("python-3.11/7", "python-3.11"),
            ("1337/2", "1337"),
        ] {
            let parsed = BuildRef::parse_id(input).expect(input);
            assert_eq!(parsed.pkgbase, pkgbase, "{input}");
        }
    }

    /// A bare number is the old id form, and silently guessing what it meant
    /// would resolve to a different build than the caller intended.
    #[test]
    fn a_bare_number_is_rejected() {
        assert!(BuildRef::parse_id("417").is_err());
        assert!(BuildRef::parse_id("hello").is_err());
        assert!(BuildRef::parse_id("/3").is_err());
        assert!(BuildRef::parse_id("hello/").is_err());
        assert!(BuildRef::parse_id("hello/x").is_err());
        // Numbering starts at 1, so 0 is not a build.
        assert!(BuildRef::parse_id("hello/0").is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClientConfig;

    #[test]
    fn parse_key_val_requires_separator() {
        assert!(parse_key_val("limit=10").is_ok());
        assert!(parse_key_val("missing").is_err());
    }

    #[test]
    fn empty_token_can_exist_in_config() {
        let config = ClientConfig {
            url: Some("http://localhost:8080/api".to_string()),
            token: Some(String::new()),
        };
        assert_eq!(config.token.as_deref(), Some(""));
    }

    #[test]
    fn add_aur_accepts_multiple_names() {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: AddPackageArgs,
        }

        let parsed = Wrapper::parse_from([
            "test",
            "paru",
            "yay",
            "--platform",
            "x86_64",
            "--build-flag=--noconfirm",
        ]);

        assert_eq!(parsed.args.packages, vec!["paru", "yay"]);
        assert_eq!(parsed.args.platforms, vec!["x86_64"]);
        assert_eq!(parsed.args.build_flags, vec!["--noconfirm"]);
    }

    /// `--from-installed` supplies the list, so requiring a positional one too
    /// would make the flag impossible to use on its own.
    #[test]
    fn from_installed_stands_in_for_the_package_list() {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: AddPackageArgs,
        }

        let parsed = Wrapper::parse_from(["test", "--from-installed", "--yes"]);
        assert!(parsed.args.packages.is_empty());
        assert!(parsed.args.from_installed);
        assert!(parsed.args.yes);
    }

    /// Without it, naming nothing is still a usage error rather than a no-op.
    #[test]
    fn a_bare_add_still_requires_a_package() {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: AddPackageArgs,
        }

        assert!(Wrapper::try_parse_from(["test"]).is_err());
    }

    /// `pkg rm $(pkg list -q)` is the reason both sides exist: the list prints
    /// bare names and the removal takes as many as it is given.
    #[test]
    fn rm_takes_the_names_list_prints() {
        let cli = Cli::parse_from(["aurcli", "pkg", "list", "-q"]);
        let Command::Pkg {
            command: PackagesCommand::List(args),
        } = cli.command
        else {
            panic!("expected pkg list");
        };
        assert!(args.quiet);

        let cli = Cli::parse_from(["aurcli", "pkg", "rm", "paru", "yay"]);
        let Command::Pkg {
            command: PackagesCommand::Rm { pkgbases },
        } = cli.command
        else {
            panic!("expected pkg rm");
        };
        assert_eq!(pkgbases, vec!["paru", "yay"]);
    }

    /// `pause --wait` takes an optional timeout, which means nothing without
    /// `--wait`; a plain `pause` is unchanged.
    #[test]
    fn pause_waits_only_when_asked() {
        let cli = Cli::parse_from(["aurcli", "worker", "pause", "4", "--wait"]);
        let Command::Worker {
            command:
                WorkerCommand::Pause {
                    id: 4,
                    wait: true,
                    wait_timeout: None,
                },
        } = cli.command
        else {
            panic!("parsed as {:?}", cli.command);
        };
        let cli = Cli::parse_from([
            "aurcli",
            "worker",
            "pause",
            "4",
            "--wait",
            "--wait-timeout",
            "600",
        ]);
        assert!(matches!(
            cli.command,
            Command::Worker {
                command: WorkerCommand::Pause {
                    wait_timeout: Some(600),
                    ..
                }
            }
        ));
        assert!(
            Cli::try_parse_from(["aurcli", "worker", "pause", "4", "--wait-timeout", "600"])
                .is_err(),
            "a timeout without --wait is a mistake worth saying"
        );
    }

    /// States are named by key, comma-separated; a misspelled one is refused
    /// before anything is asked of the server.
    #[test]
    fn builds_list_filters_by_worker_and_state() {
        let cli = Cli::parse_from([
            "aurcli",
            "builds",
            "list",
            "--worker",
            "freyja",
            "--status",
            "active,publishing",
        ]);
        let Command::Builds {
            command: BuildsCommand::List(args),
        } = cli.command
        else {
            panic!("parsed as {:?}", cli.command);
        };
        assert_eq!(args.worker.as_deref(), Some("freyja"));
        assert_eq!(args.status, [BuildState::Active, BuildState::Publishing]);
        assert!(Cli::try_parse_from(["aurcli", "builds", "list", "--status", "running"]).is_err());
    }

    /// One backing at a time, reserving only for an image, and a size that is
    /// one -- all refused before anything is written.
    #[test]
    fn pool_options_are_checked_when_parsed() {
        let base = ["aurcli", "setup", "compose", "--role", "worker", "-o", "-"];
        let parses =
            |extra: &[&str]| Cli::try_parse_from(base.iter().chain(extra.iter()).copied()).is_ok();
        assert!(parses(&["--pool-device", "/dev/zd0", "--disk-max", "500G"]));
        assert!(parses(&["--pool-image", "--disk-reserve"]));
        assert!(!parses(&[
            "--pool-device",
            "/dev/zd0",
            "--pool-mount",
            "/srv"
        ]));
        assert!(!parses(&["--pool-mount", "/srv", "--disk-reserve"]));
        assert!(!parses(&["--disk-max", "lots"]));
    }

    /// Naming nothing is a usage error, not a silent no-op.
    #[test]
    fn rm_still_requires_a_package() {
        assert!(Cli::try_parse_from(["aurcli", "pkg", "rm"]).is_err());
    }

    /// The old name keeps working for anything already scripted against it.
    #[test]
    fn delete_stays_an_alias_for_rm() {
        let cli = Cli::parse_from(["aurcli", "pkg", "delete", "paru"]);
        let Command::Pkg {
            command: PackagesCommand::Rm { pkgbases },
        } = cli.command
        else {
            panic!("expected pkg rm");
        };
        assert_eq!(pkgbases, vec!["paru"]);
    }

    #[test]
    fn repo_config_takes_the_publishing_knobs() {
        let cli = Cli::parse_from([
            "aurcli", "repo", "config", "--port", "9000", "--name", "mine",
        ]);
        let Command::Repo {
            command: RepoCommand::Config(args),
        } = cli.command
        else {
            panic!("expected repo config");
        };
        assert_eq!(args.port, Some(9000));
        assert_eq!(args.name.as_deref(), Some("mine"));
    }

    #[test]
    fn completions_name_a_shell() {
        let cli = Cli::parse_from(["aurcli", "completions", "bash"]);
        assert!(matches!(cli.command, Command::Completions { .. }));
        assert!(Cli::try_parse_from(["aurcli", "completions", "smash"]).is_err());
    }

    #[test]
    fn doctor_takes_no_arguments() {
        let cli = Cli::parse_from(["aurcli", "doctor"]);
        assert!(matches!(cli.command, Command::Doctor));
    }

    #[test]
    fn setup_worker_collects_repeated_architectures() {
        let cli = Cli::parse_from([
            "aurcli",
            "setup",
            "worker",
            "--dry-run",
            "--arch",
            "aarch64",
            "--arch",
            "armv7h",
        ]);
        let Command::Setup { command } = cli.command else {
            panic!("expected setup");
        };
        let SetupCommand::Worker(args) = *command else {
            panic!("expected worker");
        };
        assert!(args.dry_run);
        assert_eq!(args.worker.arches, vec!["aarch64", "armv7h"]);
    }

    /// Everything after `--` belongs to docker, not to us.
    #[test]
    fn setup_passes_trailing_arguments_through_to_docker() {
        let cli = Cli::parse_from(["aurcli", "setup", "server", "--", "--pull=always"]);
        let Command::Setup { command } = cli.command else {
            panic!("expected setup");
        };
        let SetupCommand::Server(args) = *command else {
            panic!("expected server");
        };
        assert_eq!(args.docker_args, vec!["--pull=always"]);
    }

    #[test]
    fn repo_config_can_install() {
        let cli = Cli::parse_from(["aurcli", "repo", "config", "--install"]);
        let Command::Repo {
            command: RepoCommand::Config(args),
        } = cli.command
        else {
            panic!("expected repo config");
        };
        assert!(args.install);
        assert_eq!(args.pacman_conf, PathBuf::from(repo::PACMAN_CONF));
    }

    #[test]
    fn packages_patch_accepts_source_patches() {
        let cli = Cli::parse_from([
            "aurcli",
            "pkg",
            "patch",
            "hello",
            "--patch",
            "PKGBUILD=./fixed-PKGBUILD",
            "--patch",
            "./other.conf",
        ]);
        let Command::Pkg {
            command: PackagesCommand::Patch(args),
        } = cli.command
        else {
            panic!("expected packages patch");
        };
        assert_eq!(args.pkgbase, "hello");
        assert_eq!(
            args.patches,
            vec![
                PatchArg {
                    source_path: "PKGBUILD".to_string(),
                    local_file: "./fixed-PKGBUILD".to_string(),
                },
                PatchArg {
                    source_path: "other.conf".to_string(),
                    local_file: "./other.conf".to_string(),
                },
            ]
        );
    }

    #[test]
    fn packages_patch_combines_patches_with_other_fields() {
        let cli = Cli::parse_from([
            "aurcli",
            "pkg",
            "patch",
            "hello",
            "--platform",
            "x86_64",
            "--patch",
            "PKGBUILD=./file",
        ]);
        let Command::Pkg {
            command: PackagesCommand::Patch(args),
        } = cli.command
        else {
            panic!("expected packages patch");
        };
        assert_eq!(args.platforms, vec!["x86_64"]);
        assert_eq!(
            args.patches,
            vec![PatchArg {
                source_path: "PKGBUILD".to_string(),
                local_file: "./file".to_string(),
            }]
        );
    }
}
