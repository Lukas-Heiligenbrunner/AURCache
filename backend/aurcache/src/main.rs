use crate::logger::init_logger;
use crate::startup::{post_startup_tasks, pre_startup_tasks};
use aurcache_api::init::{init_api, init_repo};
use aurcache_builder::init::init_build_queue;
use aurcache_db::init::init_db;
use aurcache_scheduler::auto_update::start_auto_update_job;
use aurcache_scheduler::mirror_ranking::start_mirror_rank_job;
use aurcache_scheduler::update_version_check::start_update_version_checking;
use aurcache_types::builder::Action;
use aurcache_utils::snapshot::SnapshotStore;
use dotenvy::dotenv;
use std::env;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::warn;

mod logger;
mod startup;

#[tokio::main]
async fn main() {
    _ = dotenv();
    init_logger();
    pre_startup_tasks().await;

    let (tx, _) = broadcast::channel::<Action>(32);
    let db = init_db().await.unwrap();

    let _ = post_startup_tasks(&db).await;

    // A single, long-lived `SnapshotStore` is shared across the build queue,
    // version-check loop, and auto-update job. Its persistent on-disk git
    // checkouts and `refresh()` incremental-fetch model make this safe: repeat
    // requests reuse the same checkout instead of re-cloning/re-downloading.
    let store = Arc::new(SnapshotStore::new());

    let build_queue_handle = init_build_queue(db.clone(), tx.clone(), store.clone());
    let version_check_handle = start_update_version_checking(db.clone(), store.clone());
    if let Err(e) = start_auto_update_job(db.clone(), tx.clone(), store.clone()) {
        warn!("auto_update job not properly configured: {e}");
    }

    let mirrorlist_override =
        env::var("MIRRORLIST_SERVERS_X86_64").is_ok_and(|s| !s.trim().is_empty());

    if !mirrorlist_override && let Err(e) = start_mirror_rank_job(db.clone(), tx.clone()) {
        warn!("mirror_rank job not properly configured: {e}");
    }
    let api_handle = init_api(db, tx);
    let repo_handle = init_repo();

    tokio::select! {
        _ = version_check_handle => {
            warn!("Version check handle exited");
        }
        _ = build_queue_handle => {
            warn!("Build queue handle exited");
        }
        _ = repo_handle => {
            warn!("Repo web server handle exited");
        }
        _ = api_handle => {
            warn!("API web server handle exited");
        }
    }
}
