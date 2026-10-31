//! How a request to the AUR rides out a bad answer.
//!
//! The AUR's gateway answers 502 now and then, and a version check used to
//! give up its whole pass on the first one, logging every package of the
//! batch in the URL as it did.

use aurcache_deps::AurClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// What the RPC answers when it knows none of the names asked about.
const EMPTY_ANSWER: &str = r#"{"version":5,"type":"multiinfo","resultcount":0,"results":[]}"#;

fn client_for(server: &MockServer) -> AurClient {
    AurClient::with_urls(format!("{}/rpc/v5", server.uri()))
}

/// The error and every cause under it, as the version check reports them.
fn chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();
    while let Some(error) = cause {
        message = format!("{message}: {error}");
        cause = error.source();
    }
    message
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

/// An answer that breaks off fails after the request itself succeeded, which
/// is where a version check whose answer could not be read used to log the
/// whole batch's URL: thousands of characters, every package in it.
#[tokio::test]
async fn an_unreadable_answer_does_not_list_its_packages() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = socket.read(&mut request).await;
        // Promises a longer body than it sends, then hangs up.
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{")
            .await;
    });

    let error = AurClient::with_urls(format!("http://{address}/rpc/v5"))
        .multi_info_of(&["first-package", "second-package"])
        .await
        .expect_err("the answer breaks off");

    let message = chain(&error);
    assert!(message.contains("/rpc/v5/info"), "{message}");
    assert!(!message.contains("first-package"), "{message}");
    // How much arrived of how much was promised: a cut-off connection, not an
    // answer that was whole but wrong.
    assert!(message.contains("after 1 of 1000 bytes"), "{message}");
}

/// An answer that arrives whole but is not the RPC's JSON -- a gateway's error
/// page, say -- says where it stopped making sense, not only that it did.
#[tokio::test]
async fn an_answer_that_is_not_json_says_where_it_went_wrong() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>Bad gateway</html>"))
        .mount(&server)
        .await;

    let error = client_for(&server)
        .multi_info_of(&["hello"])
        .await
        .expect_err("an HTML page is not an answer");

    let message = chain(&error);
    assert!(message.contains("line 1 column 1"), "{message}");
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
