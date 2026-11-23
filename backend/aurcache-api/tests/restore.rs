//! What the restore route answers before it starts anything.

mod common;

use aurcache_utils::dump::{build_dump, write_archive};
use rocket::http::Status;

/// A restore taking a signing key this server cannot sign with is refused
/// whole, a dry run too, rather than accepted with a failure in its report.
#[rocket::async_test]
async fn a_signing_key_this_server_cannot_use_refuses_the_restore() {
    let api = common::test_api("test", |rocket| rocket).await;
    let ca = tempfile::tempdir().unwrap();
    aurcache_ca::Ca::load_or_create(ca.path()).unwrap();
    std::fs::write(ca.path().join(aurcache_signing::KEY_FILE), "not a key").unwrap();
    let dump = write_archive(&build_dump(&api.db, "test", Some(ca.path())).await.unwrap()).unwrap();

    for dry_run in [true, false] {
        let response = api
            .client
            .post(format!("/api/restore?secrets=copy&dry_run={dry_run}"))
            .body(&dump)
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::BadRequest);
        let body = response.into_string().await.unwrap();
        let said = if dry_run {
            "nothing would be imported"
        } else {
            "nothing was imported"
        };
        assert!(
            body.contains(said) && body.contains("signing key"),
            "{body}"
        );
    }
}
