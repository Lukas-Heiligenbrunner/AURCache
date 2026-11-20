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
            "{} does not list the agent's directory, {} (add it to that file)",
            agent::SANDBOX_POLICY,
            agent::dir(data_dir).display()
        )),
    }
}

/// Say whether a build that `build_ssh_key` names no key for is kept from the
/// agent outside its chroot too, as well as inside it.
///
/// Inside the chroot it always is. Outside, it takes the sandbox, and so a
/// kernel and a policy that can: where either cannot, a worker without
/// `WORKER_GIT_SSH_PACKAGES` still builds with its key, and says what that
/// leaves open.
fn announce_host_side_gate(data_dir: &Path, gate: agent::HostSideGate) {
    match gate_gap(data_dir, gate) {
        None => tracing::info!(
            "Builds not offered the SSH key are refused its agent outside their chroot \
             as well (Landlock)"
        ),
        Some(gap) => tracing::warn!(
            "Builds are not kept from the SSH agent outside their chroot, because {gap}: \
             build_ssh_key keeps the key out of the chroot of a build it names none for, \
             but any build on this worker can reach it from its PKGBUILD code outside \
             the chroot"
        ),
    }
}

/// Start the build ssh-agent holding the key, and say what builds are
/// offered.
///
/// `None` is a worker that holds no key: every build goes ahead without one.
async fn start_agent(core: &CoreConfig, cfg: &Config) -> Option<agent::BuildAgent> {
    let gate = agent::host_side_gate(&core.data_dir);
    match agent::start(
        &core.data_dir,
        credentials::resolve(cfg).path(),
        &cfg.build_user,
    )
    .await
    {
        Ok(Some(a)) => {
            tracing::info!(
                "Build credential available through {}",
                a.socket().display()
            );
            announce_host_side_gate(&core.data_dir, gate);
            Some(a)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!("Could not start the build ssh-agent ({e:#}); {WITHOUT_KEY}");
            None
        }
    }
}

async fn run(core: CoreConfig, cfg: Config) -> Result<()> {
    // What a previous run left in the storage pool is swept when the
    // executor opens it, before anything can claim work.

    let identity = Identity::load_or_create(&core.data_dir)?;
    announce_build_credential(&cfg).await;

    // After the key is ensured, so a first start has one to load rather than
    // needing a restart. Held for the worker's lifetime: dropping it kills the
    // agent, so no stray agent outlives us holding a credential. The key is
    // read once, here: replacing it takes effect at the next restart.
    let agent = start_agent(&core, &cfg).await;

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

    // The agent's socket goes to the executor, never into this process's
    // environment: builds are offered the agent this worker started, and
    // never one an inherited `SSH_AUTH_SOCK` names.
    let ssh_keys: BTreeMap<String, PathBuf> = agent
        .iter()
        .map(|a| {
            (
                aurcache_common::ssh_key::DEFAULT.to_string(),
                a.socket().to_path_buf(),
            )
        })
        .collect();
    let executor = ChrootExecutor::new(cfg, ssh_keys).await;
    Runner::new(core, client, executor).run().await
}
