//! The server the API tests drive.

use std::sync::Arc;

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_utils::repository::Repository;
use aurcache_utils::snapshot::SnapshotStore;
use rocket::local::asynchronous::Client;
use rocket::{Build, Rocket};
use sea_orm::DatabaseConnection;

/// A client for the API, and the database behind it.
pub struct TestApi {
    pub client: Client,
    /// Allowed to go unread: this module is compiled into every test crate,
    /// and one that only makes requests never seeds anything.
    #[allow(dead_code)]
    pub db: DatabaseConnection,
}

/// The *real* route table over a fresh in-memory database, reporting `version`
/// as the server's release.
///
/// The real one so a test exercises the URL matching and decoding production
/// does rather than a reduced stand-in. That means supplying every piece of
/// state the mounted routes declare, even the ones a test never calls: Rocket
/// verifies that up front. `mount` adds whatever is mounted after the API, in
/// the order production mounts it.
pub async fn test_api(
    version: &str,
    mount: impl FnOnce(Rocket<Build>) -> Rocket<Build>,
) -> TestApi {
    test_api_with_repository(version, |repo| repo, mount).await
}

/// [`test_api`], with the repository `repository` makes of the default one.
///
/// Allowed to go unused: this module is compiled into every test crate, and
/// most never need a repository of their own.
#[allow(dead_code)]
pub async fn test_api_with_repository(
    version: &str,
    repository: impl FnOnce(Repository) -> Repository,
    mount: impl FnOnce(Rocket<Build>) -> Rocket<Build>,
) -> TestApi {
    let TestRocket { rocket, db } = test_rocket(version, repository, mount).await;
    TestApi {
        client: Client::tracked(rocket).await.unwrap(),
        db,
    }
}

/// The API as [`test_api`] builds it, served over TCP on a port of its own
/// on the loopback interface, for what only a real connection shows.
///
/// The server runs until the test's runtime ends.
#[allow(dead_code)]
pub async fn serve_api(version: &str) -> ServedApi {
    let TestRocket { rocket, db } = test_rocket(version, |repo| repo, std::convert::identity).await;
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    let port_tx = std::sync::Mutex::new(Some(port_tx));
    let rocket = rocket
        .configure(
            rocket::Config::figment()
                .merge(("address", "127.0.0.1"))
                .merge(("port", 0))
                .merge(("log_level", "off"))
                .merge(("shutdown.ctrlc", false)),
        )
        .attach(rocket::fairing::AdHoc::on_liftoff("port", move |rocket| {
            let port = rocket.config().port;
            Box::pin(async move {
                if let Some(tx) = port_tx.lock().unwrap().take() {
                    let _ = tx.send(port);
                }
            })
        }));
    tokio::spawn(rocket.launch());
    let port = port_rx.await.expect("the server starts");
    ServedApi {
        url: format!("http://127.0.0.1:{port}/api"),
        db,
    }
}

/// A server [`serve_api`] started.
#[allow(dead_code)]
pub struct ServedApi {
    /// Where its API is, as a client's base URL.
    pub url: String,
    pub db: DatabaseConnection,
}

/// The rocket [`test_api`] describes, not yet launched, and its database.
struct TestRocket {
    rocket: Rocket<Build>,
    db: DatabaseConnection,
}

/// The rocket [`test_api`] describes, not yet launched.
async fn test_rocket(
    version: &str,
    repository: impl FnOnce(Repository) -> Repository,
    mount: impl FnOnce(Rocket<Build>) -> Rocket<Build>,
) -> TestRocket {
    let db = aurcache_db::testing::memory_db().await;

    let checkouts = tempfile::tempdir().expect("tempdir");
    let rocket = rocket::build()
        .manage(db.clone())
        // No OAuth: every request is signed in, as on an instance without it.
        .manage::<Option<aurcache_api::init::OAuthSettings>>(None)
        // Routes that act on packages take the bundle; the tests never reach
        // one, but Rocket refuses to launch with an unmanaged type.
        .manage(Arc::new(SnapshotStore::with_checkout_root(
            checkouts.path().to_path_buf(),
        )))
        .manage(aurcache_utils::services::Services {
            db: db.clone(),
            store: Arc::new(SnapshotStore::with_checkout_root(
                checkouts.path().to_path_buf(),
            )),
            client: Arc::new(aurcache_deps::AurClient::new()),
            repo: Arc::new(repository(Repository::new(checkouts.path().join("repo")))),
            activity: ActivityLog::discarding(),
        })
        // The dump route reports which AURCache wrote a dump. Rocket's
        // sentinels refuse to launch without it, which is the point: a route
        // needing unmanaged state would otherwise 500 in production.
        .manage(aurcache_api::init::ServerVersion(version.to_string()))
        // Dump and restore both move the CA's files, so both need to know
        // where they are. Rocket's sentinels refuse to launch without it.
        .manage(aurcache_api::init::CaDirectory(std::path::PathBuf::from(
            "/nonexistent-ca-dir",
        )))
        // Where the listeners are: the API tells clients the repository's
        // port and the worker protocol's. Rocket's sentinels refuse to launch
        // without it.
        .manage(aurcache_api::listen::ListenAddresses::default())
        .mount("/api", aurcache_api::backend::build_api());
    let rocket = mount(rocket);
    // The checkout root only has to outlive Rocket's construction; nothing in
    // these tests fetches sources.
    std::mem::forget(checkouts);
    TestRocket { rocket, db }
}
