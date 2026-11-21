//! What the restore route answers before it starts anything.

mod common;

use rocket::http::Status;

/// A dump of an instance with no packages, with its secrets when `secrets`.
async fn dump(secrets: bool) -> Vec<u8> {
    let source = aurcache_db::testing::memory_db().await;
    let ca = tempfile::tempdir().unwrap();
    aurcache_ca::Ca::load_or_create(ca.path()).unwrap();
    let sources = secrets.then(|| aurcache_utils::dump::SecretSources { ca_dir: ca.path() });
    let dump = aurcache_utils::dump::build_dump(&source, "test", sources)
        .await
        .unwrap();
    aurcache_utils::dump::write_archive(&dump).unwrap()
}

/// A restore whose secrets cannot all be put in place -- here, a CA
/// directory that is not there -- is refused before anything is written, a
/// dry run too, rather than accepted with a failure in its report.
#[rocket::async_test]
async fn a_restore_whose_secrets_cannot_be_put_in_place_is_refused() {
    let api = common::test_api("test", |rocket| rocket).await;
    let bytes = dump(true).await;
    for (dry_run, said) in [
        (true, "nothing would be imported"),
        (false, "nothing was imported"),
    ] {
        let response = api
            .client
            .post(format!("/api/restore?secrets=copy&dry_run={dry_run}"))
            .body(bytes.clone())
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::BadRequest);
        let body = response.into_string().await.unwrap();
        assert!(body.contains(said), "{body}");
    }
}

/// A dry run that takes no secret answers as a 0.6.0 server did: no
/// `secrets` at all.
#[rocket::async_test]
async fn a_dry_run_without_secrets_answers_as_before() {
    let api = common::test_api("test", |rocket| rocket).await;
    let response = api
        .client
        .post("/api/restore?dry_run=true")
        .body(dump(false).await)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Accepted);
    let body: serde_json::Value = response.into_json().await.unwrap();
    assert!(body.get("secrets").is_none(), "{body}");
    assert_eq!(body["total"], 0);
}
