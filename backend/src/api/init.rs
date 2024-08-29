use crate::api::auth::{oauth_callback, oauth_login};
use crate::api::backend::build_api;
use crate::api::cusom_file_server::CustomFileServer;
#[cfg(feature = "static")]
use crate::api::embed::CustomHandler;
use crate::api::types::authenticated::OauthEnabled;
use crate::builder::types::Action;
use crate::utils::oauth_config::oauth_config_from_env;
use log::{error, info};
use rocket::fairing::AdHoc;
use rocket::{routes, Config};
use rocket_oauth2::{HyperRustlsAdapter, OAuth2};
use rocket_okapi::swagger_ui::{make_swagger_ui, SwaggerUIConfig};
use sea_orm::DatabaseConnection;
use tokio::sync::broadcast::Sender;
use tokio::task::JoinHandle;

pub fn init_api(db: DatabaseConnection, tx: Sender<Action>) -> JoinHandle<()> {
    tokio::spawn(async {
        let config = Config {
            address: "0.0.0.0".parse().unwrap(),
            port: 8081,
            //log_level: LogLevel::Off,
            ..Default::default()
        };

        let oauth_config = oauth_config_from_env();
        let mut rock = rocket::custom(config)
            .manage(db)
            .manage(tx)
            .manage(OauthEnabled(oauth_config.is_ok()))
            .mount("/api/", build_api())
            .mount(
                "/docs/",
                make_swagger_ui(&SwaggerUIConfig {
                    url: "../api/openapi.json".to_owned(),
                    ..Default::default()
                }),
            );

        if let Ok(oauth_config) = oauth_config {
            rock = rock
                .mount("/api/", routes![oauth_login, oauth_callback])
                .attach(AdHoc::on_ignite("OAuth Config", |rocket| async {
                    rocket.attach(OAuth2::<()>::custom(
                        HyperRustlsAdapter::default(),
                        oauth_config,
                    ))
                }));
        }

        #[cfg(feature = "static")]
        let rock = rock.mount("/", CustomHandler {});

        let rock = rock.launch().await;
        match rock {
            Ok(_) => info!("Rocket shut down gracefully."),
            Err(err) => error!("Rocket had an error: {}", err),
        };
    })
}

pub fn init_repo() -> JoinHandle<()> {
    tokio::spawn(async {
        let config = Config {
            address: "0.0.0.0".parse().unwrap(),
            port: 8080,
            //log_level: LogLevel::Off,
            ..Default::default()
        };

        let launch_result = rocket::custom(config)
            .mount("/", CustomFileServer::from("./repo"))
            .launch()
            .await;
        match launch_result {
            Ok(_) => info!("Rocket shut down gracefully."),
            Err(err) => error!("Rocket had an error: {}", err),
        };
    })
}
