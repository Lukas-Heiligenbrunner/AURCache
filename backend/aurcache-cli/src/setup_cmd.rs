//! `setup`: standing an instance up -- the arguments a compose file or a
//! `docker run` is generated from.

use crate::cli::{
    ComposeArgs, OutputFormat, SetupCommand, SetupServerArgs, SetupWorkerArgs, WorkerKnobs,
};
use crate::output::print_json;
use crate::{compose, config, setup};
use anyhow::{Context, Result, anyhow, bail};
use aurcache_common::repo::host_from_url;
use dialoguer::{Confirm, Select};
use serde_json::json;
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
            emulated_arches: self.emulated_arches.clone(),
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

pub(crate) fn run_setup_compose(format: OutputFormat, args: ComposeArgs) -> Result<()> {
    let database = compose_database(&args, config::is_interactive())?;
    let defaults = compose::ComposeParams::default();
    let params = compose::ComposeParams {
        role: args.role,
        database: database.clone(),
        server_image: args.common.server_image.unwrap_or(defaults.server_image),
        worker_image: args.common.worker_image.unwrap_or(defaults.worker_image),
        public_url: args.public_url.unwrap_or(defaults.public_url),
        log_level: args.common.log_level,
        tls_sans: args.tls_sans.unwrap_or(defaults.tls_sans),
        worker: args
            .worker
            .to_env(args.role.has_worker() && config::is_interactive())?,
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

    match format {
        OutputFormat::Json => print_json(&json!({
            "path": target,
            "role": format!("{:?}", args.role).to_lowercase(),
            "database": args.role.has_server().then_some(match database {
                compose::ComposeDatabase::Sqlite => "sqlite",
                compose::ComposeDatabase::Postgres { .. } => "postgres",
            }),
            "postgres_upgrade": match database {
                compose::ComposeDatabase::Postgres { upgrade_step, .. } => Some(upgrade_step),
                compose::ComposeDatabase::Sqlite => None,
            },
        })),
        OutputFormat::Text => {
            println!("wrote {target}");
            println!();
            println!("Start it with:");
            println!("  docker compose -f {target} up -d");
            Ok(())
        }
    }
}

pub(crate) fn run_setup_server(format: OutputFormat, args: SetupServerArgs) -> Result<()> {
    let defaults = compose::ComposeParams::default();
    let image = args.common.server_image.unwrap_or(defaults.server_image);
    let public_url = args.public_url.unwrap_or(defaults.public_url);
    let tls_sans = args.tls_sans.unwrap_or(defaults.tls_sans);

    let run = setup::server_run(
        &image,
        &public_url,
        &args.common.log_level,
        &tls_sans,
        &args.docker_args,
    );

    if args.dry_run {
        return report_dry_run(format, &run);
    }

    setup::ensure_local_objects(true)?;
    setup::execute(&run)?;

    match format {
        OutputFormat::Json => print_json(&run),
        OutputFormat::Text => {
            println!();
            println!("Server started. Next:");
            println!("  aurcli setup worker      # a build worker on this machine");
            println!("  aurcli doctor            # check it came up");
            Ok(())
        }
    }
}

pub(crate) fn run_setup_worker(format: OutputFormat, args: SetupWorkerArgs) -> Result<()> {
    let defaults = compose::ComposeParams::default();
    let image = args.common.worker_image.unwrap_or(defaults.worker_image);

    // A worker beside the server takes the bundled shortcut: shared network,
    // shared enrollment volume, no approval step. One pointed anywhere else
    // cannot, and has to establish trust explicitly.
    let local = match &args.server_url {
        None => true,
        Some(url) => host_from_url(url).is_some_and(setup::is_local_host),
    };

    let mut env = args
        .worker
        .to_env(config::is_interactive() && !args.dry_run)?;
    env.ca_fingerprint.clone_from(&args.ca_fingerprint);
    let env = if local {
        setup::local_worker_env(env)
    } else {
        let server_url = args
            .server_url
            .clone()
            .expect("a non-local worker always has a server URL");
        if env.ca_fingerprint.is_none() && env.enrollment_token.is_none() {
            eprintln!(
                "Warning: no --ca-fingerprint and no --enrollment-token. The worker will \
                 trust the server on first contact, and will wait for you to approve it \
                 on the Workers page."
            );
        }
        setup::remote_worker_env(env, &server_url)
    };

    // Each worker on a host needs its own identity volume, so the default
    // follows the container name rather than being shared.
    let data_volume = args
        .data_volume
        .unwrap_or_else(|| format!("{}_data", args.container_name.replace('-', "_")));

    let run = setup::worker_run(&setup::WorkerRunSpec {
        image,
        container_name: args.container_name,
        env,
        log_level: args.common.log_level,
        join_network: local,
        data_volume,
        extra: args.docker_args,
    });

    if args.dry_run {
        return report_dry_run(format, &run);
    }

    setup::ensure_local_objects(local)?;
    setup::execute(&run)?;

    match format {
        OutputFormat::Json => print_json(&run),
        OutputFormat::Text => {
            println!();
            if local {
                println!(
                    "Worker started, and approves itself through the shared enrollment volume."
                );
            } else {
                println!("Worker started. Approve it once it appears:");
                println!("  aurcli worker list");
            }
            println!("  aurcli doctor            # check it connected");
            println!();
            println!(
                "Its chroots, builds and caches live in one storage pool in the data volume, \
                 up to WORKER_DISK_MAX (200G by default). If Docker keeps its volumes on ZFS, \
                 see the docs' worker configuration, \"Tuning an image on ZFS\"."
            );
            Ok(())
        }
    }
}

pub(crate) fn report_dry_run(format: OutputFormat, run: &setup::DockerRun) -> Result<()> {
    match format {
        OutputFormat::Json => print_json(run),
        OutputFormat::Text => {
            println!("{}", run.command_line());
            Ok(())
        }
    }
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
    let upgrade_flag = args.postgres_upgrade || args.no_postgres_upgrade;
    if !args.role.has_server() {
        if args.database.is_some() || args.db_password.is_some() || upgrade_flag {
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
            if upgrade_flag {
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
            let upgrade_step = if args.postgres_upgrade {
                true
            } else if args.no_postgres_upgrade || !interactive {
                false
            } else {
                prompt_postgres_upgrade()?
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
