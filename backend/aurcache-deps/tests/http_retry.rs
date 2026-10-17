//! How a request to the AUR rides out a bad answer.
//!
//! The AUR's gateway answers 502 now and then, and a version check used to
//! give up its whole pass on the first one, logging every package of the
//! batch in the URL as it did.

use aurcache_deps::AurClient;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// What the RPC answers when it knows none of the names asked about.
const EMPTY_ANSWER: &str = r#"{"version":5,"type":"multiinfo","resultcount":0,"results":[]}"#;

fn client_for(server: &MockServer) -> AurClient {
    AurClient::with_urls(format!("{}/rpc/v5", server.uri()))
}

#[tokio::test]
async fn a_bad_gateway_is_retried() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(EMPTY_ANSWER))
        .mount(&server)
        .await;

    let found = client_for(&server)
        .multi_info_of(&["hello"])
        .await
        .expect("the second attempt succeeds");

    assert!(found.is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_not_found_is_an_answer_and_not_retried() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    client_for(&server)
        .multi_info_of(&["hello"])
        .await
        .expect_err("a 404 fails");

    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_failed_query_does_not_list_its_packages() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;

    let error = client_for(&server)
        .multi_info_of(&["first-package", "second-package"])
        .await
        .expect_err("every attempt fails");

    let message = error.to_string();
    assert!(message.contains("502"), "{message}");
    assert!(message.contains("/rpc/v5/info"), "{message}");
    assert!(!message.contains("first-package"), "{message}");
}

/// Every name having left the AUR is an answer: an empty one. Treated as an
/// error, it failed the whole version-check pass -- git-sourced packages
/// included -- and the packages were never recorded as gone from the AUR.
#[tokio::test]
async fn names_the_aur_does_not_know_are_an_empty_answer() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(EMPTY_ANSWER))
        .mount(&server)
        .await;

    let found = client_for(&server)
        .multi_info_of(&["gone-from-the-aur"])
        .await
        .expect("an empty result is not an error");
    assert!(found.is_empty());
}
