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
use crate::repo::init::init_repo_files;
use crate::scheduler::aur_version_update::start_aur_version_checking;
use rocket::futures::future::try_join_all;
use tokio::sync::broadcast;

fn main() {
    let t = tokio::runtime::Runtime::new().unwrap();
    t.block_on(async move {
        let (tx, _) = broadcast::channel::<Action>(32);
        let db = init_db()
            .await
            .map_err(|e| format!("Failed to initialize database: {}", e))
            .unwrap();

        init_repo_files().await.unwrap();

        let build_queue_handle = init_build_queue(db.clone(), tx.clone());
        let version_check_handle = start_aur_version_checking(db.clone());
        let api_handle = init_api(db, tx);
        let repo_handle = init_repo();
        try_join_all([
            build_queue_handle,
            version_check_handle,
            repo_handle,
            api_handle,
        ])
        .await
        .unwrap();
    });
}
