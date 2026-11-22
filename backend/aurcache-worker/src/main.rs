//! AURCache remote build worker.
//!
//! A long-lived process that enrolls with an AURCache backend over mTLS, polls
//! for build jobs, builds each package in its own `devtools` chroot, and uploads
//! the results. See `design/implemented/remote-workers.md`.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use aurcache_worker::agent;
use aurcache_worker::config::Config;
use aurcache_worker::executor::ChrootExecutor;
use aurcache_worker::settings::chroot_settings;
use aurcache_worker::{build, credentials, oneshot};
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::runner::Runner;
use aurcache_worker_core::{config::CoreConfig, enroll};
use std::collections::BTreeMap;

#[derive(Parser)]
#[command(name = "aurcache-worker", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Enroll and continuously build jobs (default).
    Run,
    /// Generate the worker identity, print its fingerprint, and exit.
    Prepare,
    /// Build a single PKGBUILD directory locally without a server (dev loop).
    BuildOnce {
        /// Path to a directory containing a PKGBUILD (defaults to cwd).
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Extra makepkg flags (e.g. --nocheck).
        #[arg(long = "flag")]
        flags: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    aurcache_worker_core::init_tracing();

    let cli = Cli::parse();
    let core = CoreConfig::from_env(
        aurcache_common::version::full_version(env!("CARGO_PKG_VERSION")),
        chroot_settings(),
    );
    let cfg = Config::from_env(&core);

    match cli.command.unwrap_or(Command::Run) {
        Command::Prepare => {
            let identity = Identity::load_or_create(&core.data_dir)?;
            println!("Worker fingerprint: {}", identity.fingerprint);
            println!("Name: {}", core.name);
            println!("Arches: {}", core.arches.join(","));
            Ok(())
        }
        Command::BuildOnce { path, flags } => {
            build::check_script().await?;
            oneshot::build_once(&cfg, &path, &flags).await
        }
        Command::Run => {
            build::check_script().await?;
            run(core, cfg).await
        }
    }
}

/// What becomes of builds on a worker that holds no SSH key, for the startup
/// lines that say it has none.
const WITHOUT_KEY: &str =
    "builds go ahead without an SSH key, and a source that needs one fails at its fetch";

/// Prepare the SSH key used for authenticated sources and log its public half.
///
/// Generation is a one-time, first-start event, but the public key is logged on
/// *every* start: it is what the operator has to paste into GitHub, and hunting
/// for it in old logs after a container restart is exactly the friction this is
/// meant to avoid.
///
/// Never fatal. A worker with no credential builds every package that does not
/// need one, which is nearly all of them.
async fn announce_build_credential(cfg: &Config) {
    let source = credentials::resolve(cfg);
    match credentials::ensure(&source).await {
        Ok(Some(public_key)) => {
            tracing::info!(
                "Build SSH key: {}\n    Add this public key to the account that \
                 may fetch restricted sources:\n    {}",
                source.path().display(),
                public_key.trim()
            );
        }
        Ok(None) => tracing::info!("Using configured build SSH key {}", source.path().display()),
        Err(e) => tracing::warn!("No build SSH key available ({e:#}); {WITHOUT_KEY}"),
    }
}

/// Why builds cannot be kept from the agent outside their chroot on this
/// machine, as a clause for the startup log; `None` when they can.
fn gate_gap(data_dir: &Path, gate: agent::HostSideGate) -> Option<String> {
    match gate {
        agent::HostSideGate::Enforced => None,
        agent::HostSideGate::KernelCannot => Some(
            "this kernel's Landlock cannot restrict UNIX sockets (that takes Landlock ABI 9, \
             Linux 7.1 or later)"
                .to_string(),
        ),
        agent::HostSideGate::PolicyMissesAgent => Some(format!(
            "{} does not list the agents' directory, {} (add it to that file)",
            agent::SANDBOX_POLICY,
            agent::dir(data_dir).display()
        )),
    }
}

/// Start an agent for each key this worker holds, and say what each is for.
///
/// The default key's always, when there is one: every build may have it
/// anyway. The named ones only where builds are kept from them outside their
/// chroot as well, which takes the sandbox, and so a kernel and a policy that
/// can: where either cannot, a build could reach every agent by its path, and
/// a key only some packages may use would be within reach of all of them. The
/// worker then holds none of them, and says why.
///
/// Held for the worker's lifetime: dropping one kills its agent, so no stray
/// agent outlives the worker holding a credential.
async fn start_agents(core: &CoreConfig, cfg: &Config) -> Vec<agent::BuildAgent> {
    let mut agents = Vec::new();
    let default = credentials::resolve(cfg);
    agents.extend(start_agent(core, cfg, aurcache_common::ssh_key::DEFAULT, default.path()).await);
    if cfg.named_ssh_keys.is_empty() {
        return agents;
    }
    let gate = agent::host_side_gate(&core.data_dir);
    match gate_gap(&core.data_dir, gate) {
        None => {
            for key in &cfg.named_ssh_keys {
                agents.extend(start_agent(core, cfg, &key.name, &key.path).await);
            }
            tracing::info!(
                "Builds are refused the agents of the SSH keys they are not offered outside \
                 their chroot as well (Landlock)"
            );
        }
        Some(gap) => tracing::error!(
            "WORKER_GIT_SSH_KEYS names SSH keys, but builds could not be kept from their agents \
             outside their chroot, because {gap}: this worker holds none of them, only its \
             default key, and a build whose package names one goes ahead without it"
        ),
    }
    agents
}

/// Start the agent holding the key `name`, from `path`; `None` when it does
/// not start, which the log says.
async fn start_agent(
    core: &CoreConfig,
    cfg: &Config,
    name: &str,
    path: &Path,
) -> Option<agent::BuildAgent> {
    match agent::start(&core.data_dir, name, path, &cfg.build_user).await {
        Ok(Some(a)) => {
            tracing::info!(
                "SSH key {name:?} available to builds through {}",
                a.socket().display()
            );
            Some(a)
        }
        Ok(None) if name == aurcache_common::ssh_key::DEFAULT => None,
        Ok(None) => {
            tracing::warn!(
                "SSH key {name:?}: {} does not exist; a build whose package names it goes \
                 ahead without it",
                path.display()
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                "Could not start the ssh-agent for SSH key {name:?} ({e:#}); a build offered \
                 it goes ahead without an SSH key"
            );
            None
        }
    }
}

async fn run(core: CoreConfig, cfg: Config) -> Result<()> {
    // What a previous run left in the storage pool is swept when the
    // executor opens it, before anything can claim work.

    let identity = Identity::load_or_create(&core.data_dir)?;
    announce_build_credential(&cfg).await;
    // Before any agent loads a key, and with the default key ensured.
    credentials::check_key_files(
        credentials::resolve(&cfg).path(),
        &cfg.named_ssh_keys,
        credentials::KeyReaders::of(&cfg.build_user),
    )?;

    // After the key is ensured, so a first start has one to load rather than
    // needing a restart. The keys are read once, here: replacing one takes
    // effect at the next restart.
    let agents = start_agents(&core, &cfg).await;

    // Waits out a server that is not up yet; see `enroll::enroll`.
    let client = enroll::enroll(&core, &identity, ChrootExecutor::KIND).await?;

    // Before any build: a crafted PKGBUILD can reach the worker-protocol port
    // from inside a build (shared network namespace) and submit a rogue
    // registration, so the build user is firewalled off it first. After
    // enrollment, because the repository the rule must leave open comes from
    // the server. Best-effort like the credential announcement: a worker that
    // cannot set the rule still builds, but says so loudly.
    match aurcache_worker::build_firewall::ensure(
        &core.aurcache_url,
        client.repo(),
        &cfg.build_user,
    ) {
        Ok(()) => tracing::info!("Build user firewalled off the worker-protocol port"),
        Err(e) => tracing::warn!(
            "Build-user worker-port rule not installed ({e:#}); builds can reach \
             the worker protocol port"
        ),
    }

    // The agents' sockets go to the executor, never into this process's
    // environment: builds are offered the agents this worker started, and
    // never one an inherited `SSH_AUTH_SOCK` names.
    let ssh_keys: BTreeMap<String, PathBuf> = agents
        .iter()
        .map(|agent| (agent.name().to_string(), agent.socket().to_path_buf()))
        .collect();
    let executor = ChrootExecutor::new(cfg, ssh_keys).await;
    Runner::new(core, client, executor).run().await
}
