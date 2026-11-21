use crate::logger::init_logger;
use crate::startup::pre_startup_tasks;
use aurcache_activitylog::activity_utils as activitylog;
use aurcache_activitylog::events::Event;
use aurcache_api::init::{CaDirectory, ServerVersion, init_api, init_repo, init_worker_api};
use aurcache_api::listen::ListenAddresses;
use aurcache_db::init::init_db;
use aurcache_deps::AurClient;
use aurcache_scheduler::activity_retention::start_activity_retention;
use aurcache_scheduler::auto_update::start_auto_update_job;
use aurcache_scheduler::lease_reaper::start_lease_reaper;
use aurcache_scheduler::mirrorlist;
use aurcache_scheduler::official_repos::start_official_repo_refresh;
use aurcache_scheduler::retired_packages::start_retired_package_sweep;
use aurcache_scheduler::signing_backfill::start_signing_backfill;
use aurcache_scheduler::signing_expiry::start_expiry_warning;
use aurcache_scheduler::update_version_check::start_update_version_checking;
use aurcache_utils::repository::{REPO_ROOT, Repository};
use aurcache_utils::services::Services;
use aurcache_utils::snapshot::SnapshotStore;
use dotenvy::dotenv;
use std::sync::Arc;
use tracing::warn;

mod logger;
mod startup;

#[tokio::main]
async fn main() {
    _ = dotenv();
    init_logger();
    // Before anything else: a listener address or port that does not parse is a
    // configuration mistake, and better refused now than after migrations ran
    // and the other listeners came up.
    let listen = ListenAddresses::from_env().unwrap_or_else(|e| {
        tracing::error!("{e}");
        std::process::exit(1);
    });
    let repo_name = match aurcache_common::server_env::AURCACHE_REPO_NAME.get() {
        Ok(name) => name,
        Err(std::env::VarError::NotPresent) => aurcache_common::repo::DEFAULT_REPO_NAME.to_string(),
        Err(error) => {
            tracing::error!("invalid AURCACHE_REPO_NAME: {error}");
            std::process::exit(1);
        }
    };
    let repo = Repository::new(REPO_ROOT)
        .with_name(repo_name)
        .unwrap_or_else(|error| {
            tracing::error!("invalid AURCACHE_REPO_NAME: {error}");
            std::process::exit(1);
        });
    let version = aurcache_common::version::full_version(env!("CARGO_PKG_VERSION"));
    pre_startup_tasks(&version).expect("could not take over the hybrid image's CA and build logs");

    let db = init_db().await.expect("failed to initialize database");

    startup::close_orphaned_operations(&db).await;
    if let Err(e) = mirrorlist::prepare().await {
        warn!("Could not write the configured mirrorlists: {e:#}");
    }

    // What 0.5.0's activity log recorded, carried into the structured log.
    // Once, in practice: rows leave the old table as they land.
    match aurcache_activitylog::legacy::import_activities(&db).await {
        Ok(0) => {}
        Ok(moved) => tracing::info!("Moved {moved} activity log entries into the log"),
        Err(e) => warn!("Could not move the activity log into the log: {e}"),
    }

    // The one task that writes the log. Everything else records through a
    // handle to it and never touches the table, so no call site has to decide
    // what to do about a write that did not land.
    let activitylog::Writer {
        log: activity,
        task: activity_writer_handle,
    } = activitylog::spawn(db.clone());

    // A line for the process starting. It says which version came up, which is
    // what lines a deploy up against whatever happened after it -- and it is
    // the marker the "since the last restart" view counts back to.
    activity.emit(Event::ServerStarted {
        version: version.clone(),
    });

    // Load (or create on first run) the internal CA used to authenticate remote
    // build workers over mutual TLS. Persisted under the data directory.
    let ca_dir = aurcache_common::fs::ca_dir();
    let ca = aurcache_ca::Ca::load_or_create(&ca_dir).expect("failed to initialize internal CA");
    if let Ok(fp) = ca.ca_cert_fingerprint() {
        tracing::info!("Worker CA fingerprint (pin this on workers): {fp}");
    }
    startup::report_untrusted_workers(&db, &ca, &activity).await;
    // Before anything can claim a build or edit a package.
    startup::settle_build_flags(&db, &activity).await;

    // A single, long-lived `SnapshotStore` shared by every path that resolves
    // package sources: the version-check loop, the auto-update job, the API/UI
    // listener (source browsing + patch editing), and the worker protocol
    // listener (job descriptors built during `claim`). Its persistent on-disk
    // git checkouts and `refresh()` incremental-fetch model make this safe:
    // repeat requests reuse the same checkout instead of re-cloning/
    // re-downloading. Handing any of these its own instance would put two
    // stores on the same checkout directories with no shared locking.
    // The database and the SSH key come along so the store can read
    // `parse_network` when it parses a PKGBUILD, and clone a private source;
    // without the database every parse would use the default.
    let ssh = Arc::new(aurcache_utils::ssh::SshAccess::load(activity.clone()));
    let store = Arc::new(SnapshotStore::new().with_db(db.clone()).with_ssh(ssh));

    // One AUR client for the whole server, for the same reason as the store
    // above: it owns the cached official repository databases and the names
    // read from them, and a second instance would fetch the same databases
    // again and hold a second answer to the same question.
    //
    // It starts with nothing read. The refresh job below fills it in and keeps
    // it current; until the first pass succeeds, dependency resolution says so
    // rather than reporting an empty repository -- so the API and the UI come
    // up either way and can report why an add failed.
    let client = Arc::new(AurClient::new());
    let official_repo_handle = start_official_repo_refresh(Arc::clone(&client), activity.clone());

    // The key the repository is signed with, when `AURCACHE_SIGN` asks for
    // one. Fatal when it cannot be loaded: carrying on unsigned would take
    // the databases' signatures down, and every client that requires them
    // with it. One that stops signing soon loads, and the activity log says
    // so (below): the server goes on serving what it signed.
    let signing_key =
        aurcache_signing::from_env(&ca_dir).expect("could not load the repository signing key");
    if let Some(key) = &signing_key {
        tracing::info!("Repository signing key fingerprint: {}", key.fingerprint());
    }

    // The pacman repository. One instance, because its lock is what keeps two
    // changes to it from interleaving: the worker protocol publishing builds,
    // package removals and restores all go through this one.
    let repo = repo.with_log(activity.clone());
    let repo = Arc::new(match signing_key {
        Some(key) => repo.with_key(key),
        None => repo,
    });
    // Not fatal: a server that will not start cannot say what is wrong, and the
    // next repository update and every publication retry recover again.
    if let Err(e) = repo.recover(&db).await {
        tracing::error!("Could not finish the last repository update: {e:#}");
    }
    if let Err(error) = repo.init() {
        tracing::error!("failed to initialize the pacman repository: {error:#}");
        std::process::exit(1);
    }

    // The things a package operation acts through, from here on passed as one.
    // Cloning is a few refcount bumps, so a job or a request handler takes its
    // own handle on the same instances.
    let services = Services {
        db: db.clone(),
        store: Arc::clone(&store),
        client,
        repo: Arc::clone(&repo),
        activity: activity.clone(),
    };

    // Builds a restart caught mid-publish pick up where they were.
    startup::resume_publishing(&db, &repo, &activity).await;
    startup::warn_about_shared_names(&db).await;

    // What was published before the repository signed, or under another
    // key, is signed in the background; until it is, the repository says so.
    start_signing_backfill(db.clone(), Arc::clone(&repo));
    // A key that stops signing soon is in the activity log: now, or once it
    // is close if the server is still running then.
    start_expiry_warning(Arc::clone(&repo));

    // Before anything else can resolve a source: a prune cannot distinguish a
    // clone in flight from a stranded one.
    startup::prune_source_checkouts(&db, &store).await;

    // What should be building and has no build gets one -- after the
    // dependency backfill an upgrade from 0.5.0 queued, if there is one.
    let backfill_done = aurcache_utils::package::backfill::start(&services).await;

    // After an upgrade's backfill: until it has recorded the names a package
    // from 0.5.0 builds, the check asks the AUR by the package's base, which
    // for a split package names nothing, and reports it removed.
    let version_check_handle = {
        let services = services.clone();
        let mut backfill_done = backfill_done;
        tokio::spawn(async move {
            let _ = backfill_done.wait_for(|done| *done).await;
            let _ = start_update_version_checking(services).await;
        })
    };
    let auto_update_handle = start_auto_update_job(services.clone());

    // Detached rather than selected on below: with ranking off there is no task.
    if let Err(e) = mirrorlist::start_ranking(activity.clone()) {
        warn!("mirror_rank job not properly configured: {e}");
    }

    // Reclaim build jobs whose remote worker went silent (lease liveness).
    let lease_reaper_handle = start_lease_reaper(db.clone(), activity.clone());

    // Delete package files the repository stopped listing, once clients with
    // an older repo.db have had time to fetch them.
    let retired_sweep_handle = start_retired_package_sweep(db.clone(), Arc::clone(&repo));

    // Keep the log from growing for ever: it records restarts and failures as
    // well as what people did.
    let activity_retention_handle = start_activity_retention(db.clone());

    let api_handle = init_api(
        services.clone(),
        ServerVersion(version),
        CaDirectory(ca_dir.clone()),
        listen,
    );
    let worker_api_handle = init_worker_api(db, ca, store, Arc::clone(&repo), activity, listen);
    let repo_handle = init_repo(repo, listen.repo);

    tokio::select! {
        _ = version_check_handle => {
            warn!("Version check handle exited");
        }
        _ = auto_update_handle => {
            warn!("Auto update handle exited");
        }
        _ = lease_reaper_handle => {
            warn!("Lease reaper handle exited");
        }
        _ = retired_sweep_handle => {
            warn!("Retired package sweep handle exited");
        }
        _ = activity_retention_handle => {
            warn!("Activity log retention handle exited");
        }
        _ = activity_writer_handle => {
            warn!("Activity log writer exited");
        }
        _ = official_repo_handle => {
            warn!("Official repository refresh handle exited");
        }
        _ = repo_handle => {
            warn!("Repo web server handle exited");
        }
        _ = api_handle => {
            warn!("API web server handle exited");
        }
        _ = worker_api_handle => {
            warn!("Worker protocol listener exited");
        }
    }
}
