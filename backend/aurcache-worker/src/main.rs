//! AURCache remote build worker.
//!
//! A long-lived process that enrolls with an AURCache backend over mTLS, polls
//! for build jobs, builds each package in its own `devtools` chroot, and uploads
//! the results. See `design/implemented/remote-workers.md`.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

use aurcache_worker::agent;
use aurcache_worker::config::Config;
use aurcache_worker::executor::ChrootExecutor;
use aurcache_worker::{credentials, oneshot};
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::runner::{Registration, Runner};
use aurcache_worker_core::{config::CoreConfig, enroll};

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Arc::new(Config::from_env());

    match cli.command.unwrap_or(Command::Run) {
        Command::Prepare => {
            let identity = Identity::load_or_create(&cfg.core.data_dir)?;
            println!("Worker fingerprint: {}", identity.fingerprint);
            println!("Name: {}", cfg.core.name);
            println!("Native arches: {}", cfg.core.native_arches.join(","));
            Ok(())
        }
        Command::BuildOnce { path, flags } => oneshot::build_once(&cfg, &path, &flags).await,
        Command::Run => run(cfg).await,
    }
}

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
        Err(e) => tracing::warn!(
            "No build SSH credential available ({e:#}); packages with \
             authenticated sources will fail"
        ),
    }
}

async fn run(cfg: Arc<Config>) -> Result<()> {
    // What a previous run left in the storage pool is swept when the
    // executor opens it, before anything can claim work.

    let identity = Identity::load_or_create(&cfg.core.data_dir)?;
    announce_build_credential(&cfg).await;

    // After the key is ensured, so a first start has one to load rather than
    // needing a restart. Held for the worker's lifetime: dropping it kills the
    // agent, so no stray agent outlives us holding a credential.
    let _agent = match agent::start(
        &cfg.core.data_dir,
        credentials::resolve(&cfg).path(),
        &cfg.build_user,
    )
    .await
    {
        Ok(Some(a)) => {
            // Children inherit this, which is how `makechrootpkg` -- and the
            // build inside the chroot -- reach the agent.
            unsafe { std::env::set_var("SSH_AUTH_SOCK", a.socket()) };
            tracing::info!(
                "Build credential available through {}",
                a.socket().display()
            );
            Some(a)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                "Could not start the build ssh-agent ({e:#}); packages with \
                 authenticated sources will fail"
            );
            None
        }
    };
    let core: Arc<CoreConfig> = Arc::new(cfg.core.clone());

    // Waits out a server that is not up yet; see `enroll::enroll`.
    let client = enroll::enroll(&core, &identity, ChrootExecutor::KIND).await?;

    // Before any build: a crafted PKGBUILD can reach the worker-protocol port
    // from inside a build (shared network namespace) and submit a rogue
    // registration, so the build user is firewalled off it first. After
    // enrollment, because the repository the rule must leave open comes from
    // the server. Best-effort like the credential announcement: a worker that
    // cannot set the rule still builds, but says so loudly.
    match aurcache_worker::build_firewall::ensure(
        &cfg.core.aurcache_url,
        client.repo_section(),
        &cfg.build_user,
    ) {
        Ok(()) => tracing::info!("Build user firewalled off the worker-protocol port"),
        Err(e) => tracing::warn!(
            "Build-user worker-port rule not installed ({e:#}); builds can reach \
             the worker protocol port"
        ),
    }

    let executor = Arc::new(ChrootExecutor::new(cfg).await);
    // What the runner registers again with when delivered values change what
    // the server schedules this worker by.
    let registration = Registration {
        csr_pem: identity.generate_csr(&core.name)?,
        kind: ChrootExecutor::KIND,
    };
    let runner = Runner::new(core, Arc::new(client), executor, registration);
    runner.run().await
}
