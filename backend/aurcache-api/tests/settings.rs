//! What the settings routes answer, read the way the page and the CLI read
//! them.

mod common;

use common::{TestApi, test_api};
use std::convert::identity;

use aurcache_common::api::settings::SettingResponse;
use aurcache_common::build_state::BuildState;
use aurcache_common::settings::{ApplicationSettings, SettingSource};
use aurcache_db::packages;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::Packages;
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::Client;
use sea_orm::ActiveValue::Set;
use sea_orm::{DatabaseConnection, EntityTrait};

async fn seed(db: &DatabaseConnection, name: &str) {
    Packages::insert(packages::ActiveModel {
        name: Set(name.to_string()),
        status: Set(BuildState::Active),
        out_of_date: Set(false),
        upstream_version: Set(Some("1.0-1".to_string())),
        build_flags: Set(Default::default()),
        platforms: Set("x86_64".parse().unwrap()),
        source_data: Set(SourceData::Aur {
            name: name.to_string(),
        }),
        directly_requested: Set(true),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("insert package");
}

async fn patch(client: &Client, path: &str, value: &str) {
    let response = client
        .patch(path)
        .header(ContentType::JSON)
        .body(serde_json::json!({ "value": value }).to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok, "PATCH {path}");
}

async fn read<T: serde::de::DeserializeOwned>(client: &Client, path: &str) -> T {
    let response = client.get(path).dispatch().await;
    assert_eq!(response.status(), Status::Ok, "GET {path}");
    serde_json::from_str(&response.into_string().await.expect("a body")).expect("JSON")
}

/// What one scope's `build_hosts` should read as.
struct Expected {
    /// The routes' prefix: a package's, or the server's.
    scope: &'static str,
    /// What the setting on its own answers.
    value: &'static str,
    source: SettingSource,
    /// The package's own list, as the page has it.
    package: Option<&'static str>,
}

/// A package's build hosts read back as they were written, one setting at a
/// time, so writing back what was read changes nothing; the page carries the
/// package's own list and the server's it is added to.
#[rocket::async_test]
async fn build_hosts_read_back_as_written() {
    let TestApi { client, db } = test_api("test", identity).await;
    seed(&db, "hello").await;
    seed(&db, "other").await;
    patch(&client, "/api/settings/build_hosts", "distcc.lan:3632").await;
    patch(
        &client,
        "/api/package/hello/settings/build_hosts",
        "gitlab.lan:22",
    )
    .await;

    // Read, modify, write: the package's list stays its own.
    let path = "/api/package/hello/settings/build_hosts";
    let read_back: SettingResponse = read(&client, path).await;
    patch(&client, path, &read_back.value).await;

    for expected in [
        Expected {
            scope: "/api/package/hello",
            value: "gitlab.lan:22",
            source: SettingSource::Package,
            package: Some("gitlab.lan:22"),
        },
        Expected {
            scope: "/api/package/other",
            value: "distcc.lan:3632",
            source: SettingSource::Global,
            package: None,
        },
        Expected {
            scope: "/api",
            value: "distcc.lan:3632",
            source: SettingSource::Global,
            package: None,
        },
    ] {
        let scope = expected.scope;
        let one: SettingResponse = read(&client, &format!("{scope}/settings/build_hosts")).await;
        assert_eq!(one.value, expected.value, "{scope}");
        assert_eq!(one.source, expected.source, "{scope}");
        let page: ApplicationSettings = read(&client, &format!("{scope}/settings")).await;
        let hosts = page.build_hosts;
        assert_eq!(hosts.package.as_deref(), expected.package, "{scope}");
        assert_eq!(hosts.server.value, "distcc.lan:3632", "{scope}");
        assert_eq!(hosts.server.source, SettingSource::Global, "{scope}");
    }
}
