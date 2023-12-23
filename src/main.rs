mod api;
mod aur;
mod builder;
mod db;
mod pkgbuild;
mod repo;

use crate::api::{backend, repository};
use crate::aur::aur::query_aur;
use crate::builder::types::Action;
use crate::db::migration::Migrator;
use rocket::config::Config;
use rocket::futures::future::join_all;
use rocket_okapi::swagger_ui::{make_swagger_ui, SwaggerUIConfig};
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use std::fs;
use tokio::sync::broadcast;

fn main() {
    let t = tokio::runtime::Runtime::new().unwrap();

    let (tx, _) = broadcast::channel::<Action>(32);

    t.block_on(async move {
        //build_package("sea-orm-cli").await;

        let db: DatabaseConnection = Database::connect("sqlite://db.sqlite?mode=rwc")
            .await
            .unwrap();

        Migrator::up(&db, None).await.unwrap();

        // Check if the directory exists
        if !fs::metadata("./repo").is_ok() {
            // Create the directory if it does not exist
            fs::create_dir("./repo").unwrap();
        }

        let db2 = db.clone();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            builder::builder::init(db2, tx2).await;
        });

        let backend_handle = tokio::spawn(async {
            let mut config = Config::default();
            config.address = "0.0.0.0".parse().unwrap();
            config.port = 8081;

            let launch_result = rocket::custom(config)
                .manage(db)
                .manage(tx)
                .mount("/", backend::build_api())
                .mount(
                    "/docs/",
                    make_swagger_ui(&SwaggerUIConfig {
                        url: "../openapi.json".to_owned(),
                        ..Default::default()
                    }),
                )
                .launch()
                .await;
            match launch_result {
                Ok(_) => println!("Rocket shut down gracefully."),
                Err(err) => println!("Rocket had an error: {}", err),
            };
        });

        let repo_handle = tokio::spawn(async {
            let mut config = Config::default();
            config.address = "0.0.0.0".parse().unwrap();
            config.port = 8080;

            let launch_result = rocket::custom(config)
                .mount("/", repository::build_api())
                .launch()
                .await;
            match launch_result {
                Ok(_) => println!("Rocket shut down gracefully."),
                Err(err) => println!("Rocket had an error: {}", err),
            };
        });

        join_all([repo_handle, backend_handle]).await;
    });

    return;
}
