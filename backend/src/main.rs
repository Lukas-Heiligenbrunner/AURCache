mod api;
mod aur;
mod builder;
mod db;
mod pkgbuild;
mod repo;
mod utils;

use crate::api::backend;
#[cfg(feature = "static")]
use crate::api::embed::CustomHandler;
use crate::builder::types::Action;
use crate::db::migration::Migrator;
use rocket::config::Config;
use rocket::fs::FileServer;
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
        // create folder for db stuff
        if !fs::metadata("./db").is_ok() {
            fs::create_dir("./db").unwrap();
        }

        let db: DatabaseConnection = Database::connect("sqlite://db/db.sqlite?mode=rwc")
            .await
            .unwrap();

        Migrator::up(&db, None).await.unwrap();

        // create repo folder
        if !fs::metadata("./repo").is_ok() {
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

            let rock = rocket::custom(config)
                .manage(db)
                .manage(tx)
                .mount("/api/", backend::build_api())
                .mount(
                    "/docs/",
                    make_swagger_ui(&SwaggerUIConfig {
                        url: "../api/openapi.json".to_owned(),
                        ..Default::default()
                    }),
                );
            #[cfg(feature = "static")]
            let rock = rock.mount("/", CustomHandler {});

            let rock = rock.launch().await;
            match rock {
                Ok(_) => println!("Rocket shut down gracefully."),
                Err(err) => println!("Rocket had an error: {}", err),
            };
        });

        let repo_handle = tokio::spawn(async {
            let mut config = Config::default();
            config.address = "0.0.0.0".parse().unwrap();
            config.port = 8080;

            let launch_result = rocket::custom(config)
                .mount("/", FileServer::from("./repo"))
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
