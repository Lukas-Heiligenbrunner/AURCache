//! Drive name discovery through the real CLI, including its metadata request.

use std::process::Command;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn detects_names_with_or_without_a_port_override_and_honors_manual_names() {
    for (args, token, name, port) in [
        (vec![], false, "custom_name", 9081),
        (vec!["--port", "9000"], true, "custom_name", 9000),
        (vec!["--name", "manual_name"], true, "manual_name", 9081),
        (
            vec!["--name", "manual_name", "--port", "9000"],
            true,
            "manual_name",
            9000,
        ),
    ] {
        let server = MockServer::start().await;
        let mut mock = Mock::given(method("GET")).and(path("/api/repo/info"));
        if token {
            mock = mock.and(header("authorization", "Bearer test-token"));
        }
        mock.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "custom_name",
            "public_url": "http://localhost:9081",
            "listen_port": 9081
        })))
        .expect(1)
        .mount(&server)
        .await;

        let config = run_config(server.uri(), args, token).await;
        assert_eq!(config["name"], name);
        assert_eq!(config["server"], format!("http://127.0.0.1:{port}/$arch"));
    }
}

#[tokio::test]
async fn offline_manual_names_do_not_contact_the_server() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let config = run_config(
        server.uri(),
        vec!["--offline", "--name", "manual_name", "--port", "9000"],
        true,
    )
    .await;
    assert_eq!(config["name"], "manual_name");
    assert_eq!(config["server"], "http://127.0.0.1:9000/$arch");
}

#[tokio::test]
async fn manual_names_work_when_the_server_cannot_report_its_name() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/repo/info"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let config = run_config(
        server.uri(),
        vec!["--name", "manual_name", "--port", "9000"],
        false,
    )
    .await;
    assert_eq!(config["name"], "manual_name");
    assert_eq!(config["server"], "http://127.0.0.1:9000/$arch");
}

async fn run_config(server: String, args: Vec<&'static str>, token: bool) -> serde_json::Value {
    let output = tokio::task::spawn_blocking(move || {
        let config_home = tempfile::tempdir().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_aurcli"));
        command
            .env("XDG_CONFIG_HOME", config_home.path())
            .env_remove("AURCACHE_TOKEN")
            .args(["--url", &format!("{server}/api"), "--format", "json"]);
        if token {
            command.args(["--token", "test-token"]);
        }
        command
            .args(["repo", "config"])
            .args(args)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
