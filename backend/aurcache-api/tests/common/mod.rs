//! The server the API tests drive.

use std::sync::Arc;

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_db::migration::Migrator;
use aurcache_utils::snapshot::SnapshotStore;
use rocket::local::asynchronous::Client;
use rocket::{Build, Rocket};
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

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
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    let checkouts = tempfile::tempdir().expect("tempdir");
    let rocket = rocket::build()
        .manage(db.clone())
        .manage(ActivityLog::discarding())
        // Routes that act on packages take the bundle; the tests never reach
        // one, but Rocket refuses to launch with an unmanaged type.
        .manage(Arc::new(SnapshotStore::with_checkout_root(
            checkouts.path().to_path_buf(),
        )))
        .manage(aurcache_utils::services::Services::new(
            db.clone(),
            Arc::new(SnapshotStore::with_checkout_root(
                checkouts.path().to_path_buf(),
            )),
            Arc::new(aurcache_deps::AurClient::new()),
            Arc::new(aurcache_utils::repository::Repository::new(
                checkouts.path().join("repo"),
            )),
            ActivityLog::discarding(),
        ))
        // The dump route reports which AURCache wrote a dump. Rocket's
        // sentinels refuse to launch without it, which is the point: a route
        // needing unmanaged state would otherwise 500 in production.
        .manage(aurcache_api::init::ServerVersion(version.to_string()))
        // Dump and restore both move the CA's files, so both need to know
        // where they are. Rocket's sentinels refuse to launch without it.
        .manage(aurcache_api::init::CaDirectory(std::path::PathBuf::from(
            "/nonexistent-ca-dir",
        )))
        .mount("/api", aurcache_api::backend::build_api());
    let rocket = mount(rocket);
    // The checkout root only has to outlive Rocket's construction; nothing in
    // these tests fetches sources.
    std::mem::forget(checkouts);
    TestApi {
        client: Client::tracked(rocket).await.unwrap(),
        db,
    }
}
