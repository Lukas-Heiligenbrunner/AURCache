//! `aurcli setup` — stand an instance up from nothing.
//!
//! The audience is someone who has just run `cargo install aurcache-cli` and has
//! no server, no worker and no token. So nothing here talks to the API: every
//! command is offline arithmetic over flags and constants. Asking for
//! credentials to an instance that does not exist yet would be absurd, and it is
//! exactly the position a first-time user is in.
//!
//! Two ways out of that position:
//!
//! - `setup compose` writes a file to hand to Docker, or to paste into TrueNAS,
//!   Portainer, Unraid or anything else that takes a compose file.
//! - `setup server` / `setup worker` run the containers here, now, with
//!   `docker run`.
//!
//! This module turns the arguments into a compose file (through `compose`) or
//! `docker run` commands (through `docker_run`).

use crate::cli::{
    ComposeArgs, OutputFormat, SetupCommand, SetupServerArgs, SetupWorkerArgs, WorkerKnobs,
};
use crate::output::render;
use crate::url::is_loopback;
use crate::{compose, config, docker_run};
use anyhow::{Context, Result, anyhow, bail};
use aurcache_common::repo::host_from_url;
use dialoguer::{Confirm, Select};
use serde::Serialize;
use std::path::Path;

impl WorkerKnobs {
    /// The worker's environment. With `ask`, a pool not given on the command
    /// line is asked for; without, it is the default image.
    pub(crate) fn to_env(&self, ask: bool) -> Result<compose::WorkerEnv> {
        Ok(compose::WorkerEnv {
            pool: self.pool_setup(ask)?,
            url: None,
            ca_fingerprint: None,
            enrollment_token: self.enrollment_token.clone(),
            enrollment_dir: None,
            name: self.worker_name.clone(),
            arches: self.arches.clone(),
            packages: self.packages.clone(),
            concurrency: self.concurrency,
            priority: self.priority,
        })
    }

    pub(crate) fn pool_setup(&self, ask: bool) -> Result<compose::PoolSetup> {
        let chosen = self.pool_device.is_some()
            || self.pool_mount.is_some()
            || self.pool_image
            || self.disk_reserve;
        let backing = match (&self.pool_device, &self.pool_mount) {
            (Some(device), _) => compose::PoolBacking::Device(device.clone()),
            (_, Some(mount)) => compose::PoolBacking::Mount(mount.clone()),
            _ if !chosen && ask => prompt_pool_backing()?,
            _ => compose::PoolBacking::Image,
        };
        if let Some(dataset) = backing.zvol() {
            eprintln!("{}", compose::zvol_tuning(&dataset).join("\n"));
        }
        Ok(compose::PoolSetup {
            backing,
            reserve: self.disk_reserve,
            disk_max: self.disk_max.clone(),
        })
    }
}

pub(crate) fn prompt_pool_backing() -> Result<compose::PoolBacking> {
    eprintln!(
        "The worker keeps every chroot, build and cache in one storage pool, under one\n\
         disk quota. See the docs' \"Storage pool\" page for the trade-offs."
    );
    let choices = [
        "An image file in the worker's volume -- nothing to prepare (default)",
        "A block device: a zvol or a partition -- best on ZFS",
        "An existing btrfs filesystem, dedicated to the worker -- best on btrfs",
    ];
    let choice = Select::new()
        .with_prompt("What should back the storage pool?")
        .items(choices)
        .default(0)
        .interact()
        .context("failed to read the storage pool choice")?;
    let path = |prompt: &str| -> Result<String> {
        dialoguer::Input::<String>::new()
            .with_prompt(prompt)
            .interact_text()
            .context("failed to read the path")
    };
    Ok(match choice {
        1 => compose::PoolBacking::Device(path(
            "Device path on the host (e.g. /dev/zvol/tank/aurcache-worker)",
        )?),
        2 => compose::PoolBacking::Mount(path("Mount point on the host")?),
        _ => compose::PoolBacking::Image,
    })
}

pub(crate) fn run_setup_command(format: OutputFormat, command: SetupCommand) -> Result<()> {
    match command {
        SetupCommand::Compose(args) => run_setup_compose(format, args),
        SetupCommand::Server(args) => run_setup_server(format, args),
        SetupCommand::Worker(args) => run_setup_worker(format, args),
    }
}

/// What `setup compose` wrote.
#[derive(Serialize)]
pub(crate) struct WrittenCompose<'a> {
    path: &'a str,
    role: compose::ComposeRole,
    /// `None` for a worker file, which has no server.
    database: Option<compose::DatabaseKind>,
    /// `None` without PostgreSQL.
    postgres_upgrade: Option<bool>,
}

pub(crate) fn run_setup_compose(format: OutputFormat, args: ComposeArgs) -> Result<()> {
    let database = compose_database(&args, config::is_interactive())?;
    let mut worker = args
        .worker
        .to_env(args.role.has_worker() && config::is_interactive())?;
    // Only a worker on its own joins a server by address: a bundled one
    // reaches the server beside it by service name.
    if !args.role.has_server() {
        worker.url = args.join.server_url;
        worker.ca_fingerprint = args.join.ca_fingerprint;
    }
    let params = compose::ComposeParams {
        role: args.role,
        database: database.clone(),
        server_image: args.common.server_image,
        worker_image: args.common.worker_image,
        public_url: args.server.public_url,
        log_level: args.common.log_level,
        tls_sans: args.server.tls_sans,
        worker,
    };
    let rendered = compose::render_compose(&params);

    let target = args
        .output
        .unwrap_or_else(|| args.role.default_filename().to_string());
    if target == "-" {
        print!("{rendered}");
        return Ok(());
    }

    let path = Path::new(&target);
    if path.exists() && !args.force {
        bail!("{target} already exists; pass --force to overwrite it");
    }
    std::fs::write(path, &rendered).with_context(|| format!("failed to write {target}"))?;

    let written = WrittenCompose {
        path: &target,
        role: args.role,
        database: args.role.has_server().then_some(match database {
            compose::ComposeDatabase::Sqlite => compose::DatabaseKind::Sqlite,
            compose::ComposeDatabase::Postgres { .. } => compose::DatabaseKind::Postgres,
        }),
        postgres_upgrade: match database {
            compose::ComposeDatabase::Postgres { upgrade_step, .. } => Some(upgrade_step),
            compose::ComposeDatabase::Sqlite => None,
        },
    };
    render(format, &written, |written| {
        println!("wrote {}", written.path);
        println!();
        println!("Start it with:");
        println!("  docker compose -f {} up -d", written.path);
    })
}

pub(crate) fn run_setup_server(format: OutputFormat, args: SetupServerArgs) -> Result<()> {
    let run = docker_run::server_run(&docker_run::ServerRunSpec {
        image: args.common.server_image,
        public_url: args.server.public_url,
        tls_sans: args.server.tls_sans,
        log_level: args.common.log_level,
        extra: args.run.docker_args,
    });

    if args.run.dry_run {
        return report_dry_run(format, &run);
    }

    docker_run::ensure_local_objects()?;
    docker_run::execute(&run)?;

    render(format, &run, |_| {
        println!();
        println!("Server started. Next:");
        println!("  aurcli setup worker      # a build worker on this machine");
        println!("  aurcli doctor            # check it came up");
    })
}

pub(crate) fn run_setup_worker(format: OutputFormat, args: SetupWorkerArgs) -> Result<()> {
    let mut env = args
        .worker
        .to_env(config::is_interactive() && !args.run.dry_run)?;
    env.ca_fingerprint = args.join.ca_fingerprint;

    // A worker beside the server takes the bundled shortcut: shared network,
    // shared enrollment volume, no approval step. One pointed anywhere else
    // cannot, and has to establish trust explicitly.
    let remote = args
        .join
        .server_url
        .filter(|url| !host_from_url(url).is_some_and(is_loopback));
    let local = remote.is_none();
    let env = match &remote {
        None => env.beside_server(),
        Some(server_url) => {
            if env.ca_fingerprint.is_none() && env.enrollment_token.is_none() {
                eprintln!(
                    "Warning: no --ca-fingerprint and no --enrollment-token. The worker will \
                     trust the server on first contact, and will wait for you to approve it \
                     on the Workers page."
                );
            }
            docker_run::remote_worker_env(env, server_url)
        }
    };

    // Each worker on a host needs its own identity volume, so the default
    // follows the container name rather than being shared.
    let data_volume = args
        .data_volume
        .unwrap_or_else(|| format!("{}_data", args.container_name.replace('-', "_")));

    let run = docker_run::worker_run(&docker_run::WorkerRunSpec {
        image: args.common.worker_image,
        container_name: args.container_name,
        env,
        log_level: args.common.log_level,
        join_network: local,
        data_volume,
        extra: args.run.docker_args,
    });

    if args.run.dry_run {
        return report_dry_run(format, &run);
    }

    // A remote worker joins neither the network nor the enrollment volume, so
    // it needs neither to exist.
    if local {
        docker_run::ensure_local_objects()?;
    }
    docker_run::execute(&run)?;

    render(format, &run, |_| {
        println!();
        if local {
            println!("Worker started, and approves itself through the shared enrollment volume.");
        } else {
            println!("Worker started. Approve it once it appears:");
            println!("  aurcli worker list");
        }
        println!("  aurcli doctor            # check it connected");
        println!();
        println!(
            "Its chroots, builds and caches live in one storage pool in the data volume, \
             up to WORKER_DISK_MAX ({} by default). If Docker keeps its volumes on ZFS, \
             see the docs' \"Storage pool\" page, \"Tuning an image on ZFS\".",
            compose::DEFAULT_DISK_MAX
        );
    })
}

pub(crate) fn report_dry_run(format: OutputFormat, run: &docker_run::DockerRun) -> Result<()> {
    render(format, run, |run| println!("{}", run.command_line()))
}

/// The database a compose file's server uses: the one named on the command
/// line, or the one picked at a prompt when `interactive`.
///
/// Without a terminal there is nobody to ask, and quietly picking a database
/// would decide where someone's data lives for them, so that is an error
/// naming the flag. The upgrade step is the other way round: it is opt-in, so
/// with nobody to ask it is left out.
pub(crate) fn compose_database(
    args: &ComposeArgs,
    interactive: bool,
) -> Result<compose::ComposeDatabase> {
    let upgrade = args.postgres_upgrade();
    if !args.role.has_server() {
        if args.database.is_some() || args.db_password.is_some() || upgrade.is_some() {
            bail!(
                "a worker file has no database; --database, --db-password and \
                 --[no-]postgres-upgrade are for a server"
            );
        }
        return Ok(compose::ComposeDatabase::Sqlite);
    }

    let kind = match args.database {
        Some(kind) => kind,
        None if interactive => prompt_database_kind()?,
        None => {
            bail!("choose the server's database with --database postgres or --database sqlite")
        }
    };
    match kind {
        compose::DatabaseKind::Sqlite => {
            if args.db_password.is_some() {
                bail!("--db-password is for --database postgres; SQLite has no password");
            }
            if upgrade.is_some() {
                bail!("--[no-]postgres-upgrade is for --database postgres");
            }
            Ok(compose::ComposeDatabase::Sqlite)
        }
        compose::DatabaseKind::Postgres => {
            let password = match &args.db_password {
                Some(password) if compose::is_plain_password(password) => password.clone(),
                Some(_) => bail!(
                    "--db-password may only use letters, digits and -_.~: the server puts it \
                     into a connection URL without escaping it"
                ),
                None => generate_password()?,
            };
            let upgrade_step = match upgrade {
                Some(choice) => choice,
                None if interactive => prompt_postgres_upgrade()?,
                None => false,
            };
            Ok(compose::ComposeDatabase::Postgres {
                password,
                upgrade_step,
            })
        }
    }
}

pub(crate) fn prompt_postgres_upgrade() -> Result<bool> {
    eprintln!(
        "An upgrade step runs {} before the database: when you raise PostgreSQL's\n\
         major version it backs the data up and runs pg_upgrade, and otherwise does\n\
         nothing. Without it, a new major version starts an empty database and the\n\
         upgrade is yours to do.",
        compose::POSTGRES_UPGRADE_IMAGE
    );
    Confirm::new()
        .with_prompt("Add the PostgreSQL upgrade step?")
        .default(false)
        .interact()
        .context("failed to read the upgrade step choice")
}

pub(crate) fn prompt_database_kind() -> Result<compose::DatabaseKind> {
    let choices = [
        (
            compose::DatabaseKind::Postgres,
            "PostgreSQL — a database service beside the server; for a deployment you keep",
        ),
        (
            compose::DatabaseKind::Sqlite,
            "SQLite — a single file in the server's volume; fine for trying AURCache out",
        ),
    ];
    let picked = Select::new()
        .with_prompt("Which database should the server use?")
        .items(choices.iter().map(|(_, label)| label))
        .default(0)
        .interact()
        .context("failed to read the database choice")?;
    Ok(choices[picked].0)
}

/// A password for a database nobody outside the compose network can reach:
/// 128 random bits, hex, so it needs no escaping anywhere it is written.
pub(crate) fn generate_password() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("failed to generate a password: {e}"))?;
    // One pre-sized `String`, not sixteen transient ones.
    let mut password = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        write!(password, "{b:02x}").expect("writing to a String cannot fail");
    }
    Ok(password)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command, SetupCommand};
    use clap::Parser;

    #[test]
    fn setup_compose_defaults_to_the_bundle() {
        let cli = Cli::parse_from(["aurcli", "setup", "compose"]);
        let Command::Setup { command } = cli.command else {
            panic!("expected setup");
        };
        let SetupCommand::Compose(args) = *command else {
            panic!("expected compose");
        };
        assert_eq!(args.role, compose::ComposeRole::Bundle);
        assert!(!args.force);
        // Unset, so that it is asked for rather than assumed.
        assert_eq!(args.database, None);
    }

    fn compose_args(argv: &[&str]) -> ComposeArgs {
        let cli = Cli::parse_from(["aurcli", "setup", "compose"].iter().chain(argv));
        let Command::Setup { command } = cli.command else {
            panic!("expected setup");
        };
        let SetupCommand::Compose(args) = *command else {
            panic!("expected compose");
        };
        args
    }

    /// Every call here passes `interactive: false`: under `cargo test` in a
    /// terminal, a prompt would wait for an answer nobody gives.
    fn database_for(argv: &[&str]) -> Result<compose::ComposeDatabase, anyhow::Error> {
        compose_database(&compose_args(argv), false)
    }

    #[test]
    fn a_named_database_is_used_without_asking() {
        assert_eq!(
            database_for(&["--database", "sqlite"]).unwrap(),
            compose::ComposeDatabase::Sqlite
        );
        assert_eq!(
            database_for(&[
                "--database",
                "postgres",
                "--db-password",
                "hunter2",
                "--postgres-upgrade",
            ])
            .unwrap(),
            compose::ComposeDatabase::Postgres {
                password: "hunter2".to_string(),
                upgrade_step: true,
            }
        );
    }

    /// Nobody to ask about the database is an error: which one holds the data
    /// is not something to guess.
    #[test]
    fn with_nobody_to_ask_the_database_must_be_named() {
        let err = database_for(&[]).unwrap_err().to_string();
        assert!(err.contains("--database"), "{err}");
    }

    /// The upgrade step is opt-in: without a flag and nobody to ask, it is
    /// left out, and each flag decides it without a question.
    #[test]
    fn the_upgrade_step_is_opt_in() {
        let upgrade_step = |argv: &[&str]| match database_for(argv).unwrap() {
            compose::ComposeDatabase::Postgres { upgrade_step, .. } => upgrade_step,
            compose::ComposeDatabase::Sqlite => panic!("expected postgres"),
        };
        assert!(!upgrade_step(&["--database", "postgres"]));
        assert!(upgrade_step(&[
            "--database",
            "postgres",
            "--postgres-upgrade"
        ]));
        assert!(!upgrade_step(&[
            "--database",
            "postgres",
            "--no-postgres-upgrade"
        ]));
        assert!(
            Cli::try_parse_from([
                "aurcli",
                "setup",
                "compose",
                "--postgres-upgrade",
                "--no-postgres-upgrade",
            ])
            .is_err()
        );
    }

    #[test]
    fn a_postgres_password_is_generated_fresh_and_plain() {
        let password = || match database_for(&["--database", "postgres"]).unwrap() {
            compose::ComposeDatabase::Postgres { password, .. } => password,
            compose::ComposeDatabase::Sqlite => panic!("expected postgres"),
        };
        let (first, second) = (password(), password());
        assert_eq!(first.len(), 32);
        assert!(compose::is_plain_password(&first), "{first}");
        assert_ne!(first, second);
    }

    #[test]
    fn a_password_that_would_need_escaping_is_refused() {
        assert!(database_for(&["--database", "postgres", "--db-password", "p@ss/word"]).is_err());
    }

    /// Flags that cannot mean anything are refused rather than ignored.
    #[test]
    fn database_flags_that_do_not_apply_are_refused() {
        assert!(database_for(&["--role", "worker", "--database", "postgres"]).is_err());
        assert!(database_for(&["--role", "worker", "--postgres-upgrade"]).is_err());
        assert!(database_for(&["--database", "sqlite", "--db-password", "x"]).is_err());
        assert!(database_for(&["--database", "sqlite", "--no-postgres-upgrade"]).is_err());
    }

    /// A worker file needs no answer, so it is never asked for one.
    #[test]
    fn a_worker_file_does_not_ask_for_a_database() {
        assert!(database_for(&["--role", "worker"]).is_ok());
    }
}
