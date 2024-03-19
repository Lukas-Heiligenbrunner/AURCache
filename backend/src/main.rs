mod api;
mod aur;
mod builder;
mod cusom_file_server;
mod db;
mod package;
mod pkgbuild;
mod repo;
mod scheduler;
mod utils;

use crate::api::backend;
#[cfg(feature = "static")]
use crate::api::embed::CustomHandler;
use crate::builder::types::Action;
use crate::cusom_file_server::CustomFileServer;
use crate::db::helpers::dbtype::{database_type, DbType};
use crate::db::migration::Migrator;
use crate::scheduler::aur_version_update::start_aur_version_checking;
use flate2::read::GzEncoder;
use flate2::Compression;
use rocket::config::Config;
use rocket::futures::future::join_all;
use rocket_okapi::swagger_ui::{make_swagger_ui, SwaggerUIConfig};
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use std::fs::File;
use std::{env, fs};
use tokio::fs::symlink;
use tokio::sync::broadcast;

fn main() {
    let t = tokio::runtime::Runtime::new().unwrap();

    let (tx, _) = broadcast::channel::<Action>(32);

    t.block_on(async move {
        let db: DatabaseConnection = match database_type() {
            DbType::SQLITE => {
                // create folder for db stuff
                if fs::metadata("./db").is_err() {
                    fs::create_dir("./db").unwrap();
                }

                Database::connect("sqlite://db/db.sqlite?mode=rwc")
                    .await
                    .expect("Failed to connect to SQLITE DB")
            }
            DbType::POSTGRES => {
                let db_user =
                    env::var("DB_USER").expect("No DB_USER envvar for POSTGRES Username specified");
                let db_pwd =
                    env::var("DB_PWD").expect("No DB_PWD envvar for POSTGRES Password specified");
                let db_host =
                    env::var("DB_HOST").expect("No DB_HOST envvar for POSTGRES HOST specified");

                Database::connect(format!(
                    "postgres://{db_user}:{db_pwd}@{db_host}/postgres?currentSchema=public"
                ))
                .await
                .expect("Failed to connect to POSTGRES DB")
            }
        };

        Migrator::up(&db, None).await.unwrap();

        // create repo folder
        if fs::metadata("./repo").is_err() {
            fs::create_dir("./repo").unwrap();

            let tar_gz = File::create("./repo/repo.db.tar.gz").unwrap();
            let enc = GzEncoder::new(tar_gz, Compression::default());
            let mut tar = tar::Builder::new(enc);
            tar.finish().expect("failed to create repo archive");
            symlink("repo.db.tar.gz", "./repo/repo.db")
                .await
                .expect("failed to create repo symlink");

            let tar_gz = File::create("./repo/repo.files.tar.gz").unwrap();
            let enc = GzEncoder::new(tar_gz, Compression::default());
            let mut tar = tar::Builder::new(enc);
            tar.finish().expect("failed to create repo archive");
            symlink("repo.files.tar.gz", "./repo/repo.files")
                .await
                .expect("failed to create repo symlink");
        }

        let db2 = db.clone();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            builder::builder::init(db2, tx2).await;
        });

        start_aur_version_checking(db.clone());

        let backend_handle = tokio::spawn(async {
            let config = Config {
                address: "0.0.0.0".parse().unwrap(),
                port: 8081,
                ..Default::default()
            };

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
            let config = Config {
                address: "0.0.0.0".parse().unwrap(),
                port: 8080,
                ..Default::default()
            };

            let launch_result = rocket::custom(config)
                .mount("/", CustomFileServer::from("./repo"))
                .launch()
                .await;
            match launch_result {
                Ok(_) => println!("Rocket shut down gracefully."),
                Err(err) => println!("Rocket had an error: {}", err),
            };
        });

        join_all([repo_handle, backend_handle]).await;
    });
}
