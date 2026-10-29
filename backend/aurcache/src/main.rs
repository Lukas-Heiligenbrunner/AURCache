use crate::logger::init_logger;
use crate::startup::{post_startup_tasks, pre_startup_tasks};
use aurcache_activitylog::activity_utils as activitylog;
use aurcache_activitylog::events::Event;
use aurcache_api::init::{CaDirectory, ServerVersion, init_api, init_repo, init_worker_api};
use aurcache_db::init::init_db;
use aurcache_deps::AurClient;
use aurcache_scheduler::activity_retention::start_activity_retention;
use aurcache_scheduler::auto_update::start_auto_update_job;
use aurcache_scheduler::lease_reaper::start_lease_reaper;
use aurcache_scheduler::mirror_ranking::start_mirror_rank_job;
use aurcache_scheduler::official_repos::start_official_repo_refresh;
use aurcache_scheduler::retired_packages::start_retired_package_sweep;
use aurcache_scheduler::update_version_check::start_update_version_checking;
use aurcache_utils::repository::{REPO_ROOT, Repository};
use aurcache_utils::services::Services;
use aurcache_utils::snapshot::SnapshotStore;
use dotenvy::dotenv;
use std::env;
use std::sync::Arc;
use tracing::warn;

mod logger;
mod startup;

#[tokio::main]
async fn main() {
    _ = dotenv();
    init_logger();
    let version = aurcache_common::version::full_version(env!("CARGO_PKG_VERSION"));
    pre_startup_tasks(&version);

    let db = init_db().await.expect("failed to initialize database");

    if let Err(e) = post_startup_tasks(&db).await {
        warn!("Startup cleanup did not complete: {e}");
    }

    // What the curated activity log recorded, carried into the structured log
    // it became. Once, in practice: rows leave the old table as they land.
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

    // A single, long-lived `SnapshotStore` shared by every path that resolves
    // package sources: the version-check loop, the auto-update job, the API/UI
    // listener (source browsing + patch editing), and the worker protocol
    // listener (job descriptors built during `claim`). Its persistent on-disk
    // git checkouts and `refresh()` incremental-fetch model make this safe:
    // repeat requests reuse the same checkout instead of re-cloning/
    // re-downloading. Handing any of these its own instance would put two
    // stores on the same checkout directories with no shared locking.
    // The database comes along so the store can read `parse_network` when it
    // parses a PKGBUILD; without it every parse would use the default.
    let store = Arc::new(SnapshotStore::new().with_db(db.clone()));

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

    // The pacman repository. One instance, because its lock is what keeps two
    // changes to it from interleaving: the worker protocol publishing builds,
    // package removals and restores all go through this one.
    let repo = Arc::new(Repository::new(REPO_ROOT).with_log(activity.clone()));
    // Not fatal: a server that will not start cannot say what is wrong, and the
    // next repository update and every publication retry recover again.
    if let Err(e) = repo.recover(&db).await {
        tracing::error!("Could not finish the last repository update: {e:#}");
    }
    if let Err(e) = repo.init() {
        tracing::error!("Failed to initialize the pacman repository: {e:#}");
    }

    // The things a package operation acts through, from here on passed as one.
    // Cloning is a few refcount bumps, so a job or a request handler takes its
    // own handle on the same instances.
    let services = Services::new(
        db.clone(),
        Arc::clone(&store),
        client,
        Arc::clone(&repo),
        activity.clone(),
    );

    // Builds a restart caught mid-publish pick up where they were.
    startup::resume_publishing(&db, &repo, &activity).await;

    // Before anything else can resolve a source: a prune cannot distinguish a
    // clone in flight from a stranded one.
    startup::prune_source_checkouts(&db, &store).await;

    // Anything that should be building and has no build -- a package added
    // while the server was down, or whose build row was lost -- gets one.
    if let Err(e) =
        aurcache_utils::package::enqueue::enqueue_missing_buildable_packages(&db, &activity).await
    {
        activity.emit(Event::StartupEnqueueFailed {
            error: format!("{e:#}"),
        });
    }

    // An upgrade from 0.5.0 queues its packages' dependencies to be recorded.
    // In the background: it resolves every package's source, and nothing
    // below waits on it. What it adds is queued to build once it is done.
    tokio::spawn({
        let services = services.clone();
        async move {
            use aurcache_utils::package::backfill::{Backfill, backfill_dependencies};
            match backfill_dependencies(&services).await {
                Ok(Backfill::Ran) => {}
                Ok(Backfill::NotQueued) => return,
                Err(e) => {
                    tracing::error!("Dependency backfill stopped: {e:#}");
                    return;
                }
            }
            if let Err(e) = aurcache_utils::package::enqueue::enqueue_missing_buildable_packages(
                &services.db,
                &services.activity,
            )
            .await
            {
                services.activity.emit(Event::StartupEnqueueFailed {
                    error: format!("{e:#}"),
                });
            }
        }
    });

    let version_check_handle = start_update_version_checking(services.clone());
    let auto_update_handle = start_auto_update_job(services.clone());

    let mirrorlist_override =
        env::var("MIRRORLIST_SERVERS_X86_64").is_ok_and(|s| !s.trim().is_empty());

    if !mirrorlist_override && let Err(e) = start_mirror_rank_job(activity.clone()) {
        warn!("mirror_rank job not properly configured: {e}");
    }

    // Reclaim build jobs whose remote worker went silent (lease liveness).
    let lease_reaper_handle = start_lease_reaper(db.clone(), activity.clone());

    // Delete package files the repository stopped listing, once clients with
    // an older repo.db have had time to fetch them.
    let retired_sweep_handle = start_retired_package_sweep(db.clone(), Arc::clone(&repo));

    // Keep the log from growing for ever. Nothing has ever pruned it, and it
    // now records restarts and failures as well as what people did.
    let activity_retention_handle = start_activity_retention(db.clone());

    let api_handle = init_api(
        services.clone(),
        ServerVersion(version),
        CaDirectory(ca_dir.clone()),
    );
    let worker_api_handle = init_worker_api(db, ca, store, Arc::clone(&repo), activity);
    let repo_handle = init_repo(repo);

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
