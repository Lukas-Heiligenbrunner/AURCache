use crate::aur::AURApi;
use crate::auth::{OauthUserInfo, oauth_callback, oauth_login};
use crate::backend::build_api;
use crate::custom_file_server::CustomFileServer;
#[cfg(feature = "static")]
use crate::embed::CustomHandler;
use crate::models::authenticated::OauthEnabled;
use crate::utils::config::{ALLOWED_USERS_ENV, allowed_users, oauth_config_from_env};
use crate::utils::failed_requests::{FailedRequests, plain_status};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_utils::repository::Repository;
use aurcache_utils::services::Services;
use aurcache_utils::snapshot::SnapshotStore;
use rocket::config::SecretKey;
use rocket::fairing::AdHoc;
use rocket::http::private::cookie::Key;
use rocket::{Config, catchers, routes};
use rocket_async_compression::{Compression, Level};
use rocket_oauth2::HyperRustlsAdapter;
use sea_orm::DatabaseConnection;
use std::env;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use utoipa::openapi::security::{AuthorizationCode, Flow, OAuth2, Scopes};
use utoipa::{Modify, OpenApi, openapi::security::SecurityScheme};
use utoipa_redoc::{Redoc, Servable as _};
use utoipa_scalar::{Scalar, Servable as _};

fn get_secret_key() -> SecretKey {
    if let Ok(secret_key) = env::var("SECRET_KEY") {
        SecretKey::from(secret_key.as_bytes())
    } else {
        warn!("`SECRET_KEY` env not set, generating random key.");
        SecretKey::from(
            Key::try_generate()
                .expect("no secure RNG available to generate a cookie key")
                .master(),
        )
    }
}

/// Build the mutual-TLS configuration for the worker protocol port: a server
/// certificate issued by the internal CA, with client certificates optional
/// (`mandatory = false`) so enrollment endpoints remain reachable without one.
fn worker_tls_config(ca: &aurcache_ca::Ca) -> Option<rocket::config::TlsConfig> {
    use rocket::config::{MutualTls, TlsConfig};

    let sans = env::var("AURCACHE_TLS_SANS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["localhost".to_string()]);

    let aurcache_ca::ServerCert { cert_pem, key_pem } = match ca.issue_server_cert(sans) {
        Ok(cert) => cert,
        Err(e) => {
            error!("Failed to issue server certificate, TLS disabled: {e}");
            return None;
        }
    };
    let ca_pem = ca.ca_cert_pem().as_bytes().to_vec();

    let tls = TlsConfig::from_bytes(cert_pem.as_bytes(), key_pem.as_bytes())
        .with_mutual(MutualTls::from_bytes(&ca_pem).mandatory(false));
    Some(tls)
}

/// Where the worker CA lives, supplied by the binary.
///
/// The CA is files on disk rather than rows, and only the binary resolves where
/// -- `AURCACHE_CA_DIR`, or `./data/ca`. Dump and restore both move those files,
/// so both need to be told where they are.
#[derive(Debug, Clone)]
pub struct CaDirectory(pub std::path::PathBuf);

/// The running server's release version, supplied by the binary.
///
/// Only the `aurcache` crate carries a meaningful version; the library crates
/// are unversioned and would report `0.0.0`. A dump records which AURCache
/// wrote it, so it has to be the real one.
#[derive(Debug, Clone)]
pub struct ServerVersion(pub String);

/// Start the human-facing API/UI listener.
///
/// `services` carries the process-wide [`SnapshotStore`]; it must be the same
/// instance handed to the schedulers and the worker listener so every path
/// shares one set of on-disk git checkouts (see `main.rs`).
#[must_use]
pub fn init_api(services: Services, version: ServerVersion, ca_dir: CaDirectory) -> JoinHandle<()> {
    tokio::spawn(async move {
        let config = Config {
            address: Ipv4Addr::UNSPECIFIED.into(),
            port: aurcache_common::ports::AURCACHE_HTTP_PORT,
            secret_key: get_secret_key(),
            ..Default::default()
        };

        #[derive(OpenApi)]
        #[openapi(
            nest(
                (path = "/api", api = AURApi, tags = ["AUR"]),
                (path = "/api", api = crate::auth::AuthApi, tags = ["Auth"]),
                (path = "/api", api = crate::build::BuildApi, tags = ["Build"]),
                (path = "/api", api = crate::health::HealthApi, tags = ["Health"]),
                (path = "/api", api = crate::repo::RepoApi, tags = ["Repo"]),
                (path = "/api", api = crate::package::PackageApi, tags = ["Package"]),
                (path = "/api", api = crate::stats::StatsApi, tags = ["Stats"]),
                (path = "/api", api = crate::dump::DumpApi, tags = ["Dump"]),
                (path = "/api", api = crate::log::LogApi, tags = ["Log"]),
                (path = "/api", api = crate::settings::SettingsApi, tags = ["Settings"]),
                (path = "/api", api = crate::worker::WorkerApi, tags = ["Worker"]),
            ),
            tags(
                (name = "AUR", description = "AUR management endpoints."),
                (name = "Build", description = "Build management endpoints."),
                (name = "Auth", description = "Authentication"),
                (name = "Health", description = "Health endpoints"),
                (name = "Package", description = "Package management endpoints."),
                (name = "Stats", description = "Statistics endpoints."),
                (name = "Activity", description = "Activity endpoints."),
                (name = "Settings", description = "Settings endpoints."),
                (name = "Worker", description = "Remote build worker protocol and management."),
            ),
            modifiers(&SecurityAddon)
        )]
        struct ApiDoc;

        struct SecurityAddon;

        impl Modify for SecurityAddon {
            fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
                let (Some(components), Ok(oauth_config)) =
                    (openapi.components.as_mut(), oauth_config_from_env())
                else {
                    return;
                };
                components.add_security_scheme(
                    "openid_connect",
                    SecurityScheme::OAuth2(OAuth2::new([Flow::AuthorizationCode(
                        AuthorizationCode::new(
                            oauth_config.provider().auth_uri(),
                            oauth_config.provider().token_uri(),
                            Scopes::new(),
                        ),
                    )])),
                );
            }
        }

        let oauth_config = oauth_config_from_env();

        // An allowlist restricts who may *sign in*, so it does nothing at all
        // without OAuth: every request is already authenticated when OAuth is
        // off. Said out loud, because the mistake looks exactly like a working
        // restriction from the outside -- the server simply lets everyone in.
        if oauth_config.is_err() && allowed_users().is_some() {
            tracing::warn!(
                "{ALLOWED_USERS_ENV} is set but OAuth is not configured, so it restricts nothing \
                 and this instance is open to everyone. Configure the OAUTH_* variables to \
                 enforce it."
            );
        }

        let mut rock = rocket::custom(config)
            // Compress here rather than only at whatever proxy is in front.
            //
            // A build log is the most compressible thing this server sends and
            // the largest: unreal-engine's reached 4.3 MB, which a phone on a
            // mobile connection could not fetch before the request timed out.
            // It compresses about 11x.
            //
            // Doing it in the application rather than in the reverse proxy
            // matters for two reasons. A deployment may have no proxy at all,
            // and where there is one it is often somewhere else entirely --
            // this instance reaches its proxy over a WireGuard tunnel, so
            // compressing at the proxy still drags the uncompressed body across
            // the wire. A proxy that compresses as well simply passes an
            // already-encoded response through.
            //
            // The repository file server is a separate Rocket (`init_repo`), so
            // this cannot touch package downloads -- which are `.pkg.tar.zst`
            // and must never be re-compressed. The fairing's own defaults also
            // skip images, video, archives and `text/event-stream`.
            .attach(Compression::with_level(Level::Precise(4)))
            .attach(FailedRequests)
            .register("/", catchers![plain_status])
            .manage(services.db.clone())
            .manage(OauthEnabled(oauth_config.is_ok()))
            .manage(services.activity.clone())
            // Also managed on their own: a route that needs one of them says
            // so, rather than asking for the bundle and using a field.
            .manage(Arc::clone(&services.store))
            .manage(services)
            .manage(version)
            .manage(ca_dir)
            .mount("/api/", build_api())
            .mount("/api/", crate::worker::worker_admin_routes())
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
        let rock = rock.mount("/", CustomHandler);

        let rock = rock.launch().await;
        match rock {
            Ok(_) => info!("Rocket shut down gracefully."),
            Err(err) => error!("Rocket had an error: {err}"),
        }
    })
}

/// Dedicated remote-worker protocol listener: HTTPS with **optional** mutual TLS
/// on `AURCACHE_WORKER_PORT` (default 8083). Enrollment endpoints are reachable
/// without a client certificate; job endpoints require an approved worker's
/// certificate. This is intentionally separate from [`init_api`] so the
/// human-facing API/UI need no TLS of their own (front them with a reverse proxy
/// if desired) while worker mTLS is scoped to exactly this surface.
///
/// If a server certificate cannot be issued from the internal CA, the listener
/// is not started and a warning is logged (workers will be unable to connect).
///
/// `store` is the process-wide [`SnapshotStore`], shared with [`init_api`] and
/// the schedulers: job descriptors built during `claim` resolve sources through
/// the same on-disk checkouts the version-check loop maintains, so the two never
/// race on (or duplicate) a clone of the same package.
#[must_use]
pub fn init_worker_api(
    db: DatabaseConnection,
    ca: aurcache_ca::Ca,
    store: Arc<SnapshotStore>,
    repo: Arc<Repository>,
    activity: ActivityLog,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(tls) = worker_tls_config(&ca) else {
            error!("Worker TLS could not be configured; worker protocol listener disabled");
            // Parked rather than returned: the server runs for as long as every
            // one of its tasks does, and the API and the repository are still
            // worth serving without workers.
            return std::future::pending().await;
        };

        let port = worker_port();

        let config = Config {
            address: Ipv4Addr::UNSPECIFIED.into(),
            port,
            secret_key: get_secret_key(),
            tls: Some(tls),
            ..Default::default()
        };

        info!("Starting remote-worker mTLS protocol listener on port {port}");
        let launch_result = rocket::custom(config)
            // A worker enrolling or being auto-approved is worth a line in the
            // log, and this listener is where both happen.
            .manage(activity)
            .attach(FailedRequests)
            .register("/", catchers![plain_status])
            .manage(db)
            .manage(ca)
            .manage(store)
            .manage(repo)
            .manage(crate::worker::AgreedProtocols::default())
            .mount("/api/", crate::worker::worker_protocol_routes())
            .launch()
            .await;
        match launch_result {
            Ok(_) => info!("Worker protocol listener shut down gracefully."),
            Err(err) => error!("Worker protocol listener had an error: {err}"),
        }
    })
}

/// The port the worker protocol listens on: `AURCACHE_WORKER_PORT`, else the
/// default.
pub(crate) fn worker_port() -> u16 {
    env::var("AURCACHE_WORKER_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(aurcache_common::ports::AURCACHE_WORKER_PORT)
}

#[must_use]
pub fn init_repo(repo: Arc<Repository>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let config = Config {
            address: Ipv4Addr::UNSPECIFIED.into(),
            port: aurcache_common::ports::AURCACHE_MIRROR_PORT,
            secret_key: get_secret_key(),
            ..Default::default()
        };

        let launch_result = rocket::custom(config)
            // A missing signature is a 404 pacman expects, so it is not
            // logged; it gets an answer without Rocket warning for each one.
            .register("/", catchers![plain_status])
            .mount("/", CustomFileServer::new(repo.root()))
            .launch()
            .await;
        match launch_result {
            Ok(_) => info!("Rocket shut down gracefully."),
            Err(err) => error!("Rocket had an error: {err}"),
        }
    })
}
