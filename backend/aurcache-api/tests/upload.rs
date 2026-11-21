//! `POST /api/package/<pkgbase>/upload`: the multipart body, the answers, and
//! what the package then looks like through the API.

mod common;
#[path = "../../aurcache-utils/tests/common/mod.rs"]
mod packages;

use aurcache_client::UploadFile;
use aurcache_common::api::package::{ExtendedPackage, PackageSource, UploadedBuild};
use aurcache_common::build_state::BuildState;
use aurcache_db::testing::package;
use common::{serve_api, test_api};
use packages::{pkg, quiet_logs};
use rocket::http::{ContentType, Status};

const BOUNDARY: &str = "upload-test-boundary";

/// A multipart body carrying `files`, one part each.
fn multipart(files: &[UploadFile]) -> Vec<u8> {
    let mut body = Vec::new();
    for UploadFile { filename, contents } in files {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
                 filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(contents);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn form_data() -> ContentType {
    ContentType::new("multipart", "form-data").with_params(("boundary", BOUNDARY))
}

#[tokio::test]
async fn an_upload_publishes_a_build_of_the_package() {
    quiet_logs();
    let api = test_api("test", std::convert::identity).await;
    package("demo").insert(&api.db).await;
    let body = multipart(&[UploadFile {
        filename: "demo-1.0-1-x86_64.pkg.tar.zst".to_string(),
        contents: pkg("demo", "1.0-1", "x86_64").bytes().into(),
    }]);

    let response = api
        .client
        .post("/api/package/demo/upload")
        .header(form_data())
        .body(body)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let uploaded: UploadedBuild = response.into_json().await.unwrap();
    assert_eq!(uploaded.status, BuildState::Successful);
    assert_eq!(uploaded.number, 1);

    let package: ExtendedPackage = api
        .client
        .get("/api/package/demo")
        .dispatch()
        .await
        .into_json()
        .await
        .unwrap();
    assert!(
        matches!(package.package_source, PackageSource::Aur(_)),
        "it is still the AUR package it was"
    );
    assert_eq!(package.built_version.as_deref(), Some("1.0-1"));
    assert_eq!(package.files.len(), 1);
}

/// One case of each answer a refusal can be.
#[tokio::test]
async fn a_refused_upload_says_why() {
    quiet_logs();
    let api = test_api("test", std::convert::identity).await;
    package("refused").insert(&api.db).await;
    let package = pkg("refused", "1.0-1", "x86_64");
    let upload = async |pkgbase: &str, files: &[UploadFile]| {
        api.client
            .post(format!("/api/package/{pkgbase}/upload"))
            .header(form_data())
            .body(multipart(files))
            .dispatch()
            .await
    };
    let file = |filename: String, contents: Vec<u8>| UploadFile {
        filename,
        contents: contents.into(),
    };

    let absent = upload("absent", &[file(package.filename(), package.bytes())]).await;
    assert_eq!(absent.status(), Status::NotFound);

    let other = pkg("other", "1.0-1", "x86_64");
    let wrong_name = upload("refused", &[file(other.filename(), other.bytes())]).await;
    assert_eq!(wrong_name.status(), Status::BadRequest);
    let reason = wrong_name.into_string().await.unwrap();
    assert!(
        reason.contains("not among the packages refused's"),
        "{reason}"
    );

    let signature = format!("{}.sig", package.filename());
    let large_signature = upload(
        "refused",
        &[
            file(package.filename(), package.bytes()),
            file(signature, vec![0x89; 17 * 1024]),
        ],
    )
    .await;
    assert_eq!(large_signature.status(), Status::PayloadTooLarge);
}

/// A part header that never ends is refused once it is larger than any
/// upload needs, rather than held in memory for as long as it is sent.
#[tokio::test]
async fn an_endless_part_header_is_cut_off() {
    quiet_logs();
    let api = test_api("test", std::convert::identity).await;
    package("endless").insert(&api.db).await;
    let mut body = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"").into_bytes();
    body.resize(body.len() + (9 << 20), b'a');

    let response = api
        .client
        .post("/api/package/endless/upload")
        .header(form_data())
        .body(body)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
    let reason = response.into_string().await.unwrap();
    assert!(reason.contains("part headers"), "{reason}");
}

/// A refusal made part-way through the body, or before any of it is read,
/// reaches a client still streaming it over a real connection.
#[tokio::test]
async fn a_refusal_reaches_a_client_still_sending() {
    quiet_logs();
    let api = serve_api("test").await;
    let client = aurcache_client::AurCacheClient::new(api.url.clone(), None).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let large = dir.path().join("sending-1.0-1-x86_64.pkg.tar.zst");
    std::fs::write(&large, vec![0x28; 64 << 20]).unwrap();
    let stray = dir.path().join("PKGBUILD");
    std::fs::write(&stray, b"pkgname=sending\n").unwrap();
    let upload = |pkgbase: &'static str, files: Vec<std::path::PathBuf>| {
        let client = client.clone();
        async move {
            match client.upload_package(pkgbase, None, &files).await {
                Ok(uploaded) => panic!("{pkgbase}: accepted as {uploaded:?}"),
                Err(e) => format!("{e:#}"),
            }
        }
    };

    package("sending").insert(&api.db).await;
    let stray_first = upload("sending", vec![stray, large.clone()]).await;
    assert!(stray_first.contains("neither a package"), "{stray_first}");

    let absent = upload("absent", vec![large]).await;
    assert!(absent.contains("no package 'absent'"), "{absent}");
}
