//! Repository metadata used by `aurcli repo config`, and the key a signing
//! repository is verified with.

mod common;

#[rocket::async_test]
async fn repository_info_reports_its_name() {
    let api = common::test_api("test", |rocket| rocket).await;
    let response = api.client.get("/api/repo/info").dispatch().await;
    assert_eq!(response.status(), rocket::http::Status::Ok);
    let info = response
        .into_json::<aurcache_common::api::repo::RepoInfo>()
        .await
        .unwrap();
    assert_eq!(info.name, "repo");
    assert_eq!(info.listen_port, Some(8081));
}

/// A signing repository serves its armored public key, as the media type
/// `pacman-key --add` is fed.
#[rocket::async_test]
async fn a_signing_repository_serves_its_public_key() {
    let key = aurcache_signing::InstanceKey::generate(None).unwrap();
    let armored = key.public_key().to_string();
    let api =
        common::test_api_with_repository("test", |repo| repo.with_key(key), |rocket| rocket).await;
    let response = api.client.get("/api/repo/key").dispatch().await;
    assert_eq!(response.status(), rocket::http::Status::Ok);
    assert_eq!(
        response.content_type(),
        Some(rocket::http::ContentType::new("application", "pgp-keys"))
    );
    assert_eq!(response.into_string().await.unwrap(), armored);
}

/// A repository that does not sign has no key to serve.
#[rocket::async_test]
async fn an_unsigned_repository_has_no_key() {
    let api = common::test_api("test", |rocket| rocket).await;
    let response = api.client.get("/api/repo/key").dispatch().await;
    assert_eq!(response.status(), rocket::http::Status::NotFound);
}
