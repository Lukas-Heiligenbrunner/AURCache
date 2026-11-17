//! Repository metadata used by `aurcli repo config`.

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
