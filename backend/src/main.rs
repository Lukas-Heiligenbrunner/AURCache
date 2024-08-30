mod api;
mod aur;
mod builder;
mod db;
mod package;
mod repo;
mod scheduler;
mod utils;

use crate::api::init::{init_api, init_repo};
use crate::builder::init::init_build_queue;
use crate::builder::types::Action;
use crate::db::init::init_db;
use crate::scheduler::aur_version_update::start_aur_version_checking;
use crate::utils::logger::init_logger;
use crate::utils::startup::startup_tasks;
use dotenvy::dotenv;
use log::warn;
use tokio::sync::broadcast;

#[tokio::main]
async fn main() {
    _ = dotenv();
    init_logger();
    startup_tasks().await.unwrap();

    let (tx, _) = broadcast::channel::<Action>(32);
    let db = init_db().await.unwrap();

    let build_queue_handle = init_build_queue(db.clone(), tx.clone());
    let version_check_handle = start_aur_version_checking(db.clone());
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
