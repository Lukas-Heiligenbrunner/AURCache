use crate::activity_log::activity_utils::ActivityLog;
use crate::api::aur::AURApi;
use crate::api::auth::{OauthUserInfo, oauth_callback, oauth_login};
use crate::api::backend::build_api;
use crate::api::cusom_file_server::CustomFileServer;
#[cfg(feature = "static")]
use crate::api::embed::CustomHandler;
use crate::api::models::authenticated::OauthEnabled;
use crate::builder::types::Action;
use crate::utils::oauth_config::oauth_config_from_env;
use log::{error, info, warn};
use rocket::config::SecretKey;
use rocket::fairing::AdHoc;
use rocket::http::private::cookie::Key;
use rocket::{Config, routes};
use rocket_oauth2::HyperRustlsAdapter;
use sea_orm::DatabaseConnection;
use std::env;
use tokio::sync::broadcast::Sender;
use tokio::task::JoinHandle;
use utoipa::openapi::security::{AuthorizationCode, Flow, OAuth2, Scopes};
use utoipa::{Modify, OpenApi, openapi::security::SecurityScheme};
use utoipa_redoc::{Redoc, Servable as _};
use utoipa_scalar::{Scalar, Servable as _};

fn get_secret_key() -> SecretKey {
    match env::var("SECRET_KEY") {
        Ok(secret_key) => SecretKey::from(secret_key.as_bytes()),
        Err(_) => {
            warn!("`SECRET_KEY` env not set, generating random key.");
            SecretKey::from(Key::try_generate().unwrap().master())
        }
    }
}

pub fn init_api(db: DatabaseConnection, tx: Sender<Action>) -> JoinHandle<()> {
    tokio::spawn(async {
        let config = Config {
            address: "0.0.0.0".parse().unwrap(),
            port: 8080,
            secret_key: get_secret_key(),
            ..Default::default()
        };

        #[derive(OpenApi)]
        #[openapi(
            nest(
                (path = "/api", api = AURApi, tags = ["AUR"]),
                (path = "/api", api = crate::api::auth::AuthApi, tags = ["Auth"]),
                (path = "/api", api = crate::api::build::BuildApi, tags = ["Build"]),
                (path = "/api", api = crate::api::health::HealthApi, tags = ["Health"]),
                (path = "/api", api = crate::api::package::PackageApi, tags = ["Package"]),
                (path = "/api", api = crate::api::stats::StatsApi, tags = ["Stats"]),
                (path = "/api", api = crate::api::activity::ActivityApi, tags = ["Activity"]),
            ),
            tags(
                (name = "AUR", description = "AUR management endpoints."),
                (name = "Build", description = "Build management endpoints."),
                (name = "Auth", description = "Authentication"),
                (name = "Health", description = "Health endpoints"),
                (name = "Package", description = "Package management endpoints."),
                (name = "Stats", description = "Statistics endpoints."),
                (name = "Activity", description = "Activity endpoints."),
            ),
            modifiers(&SecurityAddon)
        )]
        struct ApiDoc;

        struct SecurityAddon;

        impl Modify for SecurityAddon {
            fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
                let components = openapi.components.as_mut().unwrap(); // we can unwrap safely since there already is components registered.
                let oauth_config = oauth_config_from_env();
                if let Ok(oauth_config) = oauth_config {
                    components.add_security_scheme(
                        "openid_connect",
                        SecurityScheme::OAuth2(OAuth2::new([Flow::AuthorizationCode(
                            AuthorizationCode::new(
                                oauth_config.provider().auth_uri(),
                                oauth_config.provider().token_uri(),
                                Scopes::new(),
                            ),
                        )])),
                    )
                }
            }
        }

        let oauth_config = oauth_config_from_env();
        let mut rock = rocket::custom(config)
            .manage(db.clone())
            .manage(tx)
            .manage(OauthEnabled(oauth_config.is_ok()))
            .manage(ActivityLog::new(db))
            .mount("/api/", build_api())
            .mount("/", Scalar::with_url("/docs", ApiDoc::openapi()))
            .mount("/", Redoc::with_url("/redoc", ApiDoc::openapi()));

        if let Ok(oauth_config) = oauth_config {
            rock = rock
                .mount("/api/", routes![oauth_login, oauth_callback])
                .attach(AdHoc::on_ignite("OAuth Config", |rocket| async {
                    rocket.attach(rocket_oauth2::OAuth2::<OauthUserInfo>::custom(
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
            port: 8081,
            secret_key: get_secret_key(),
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
