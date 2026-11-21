//! `POST /api/package/<pkgbase>/upload`: the multipart body, the answers, and
//! what the package then looks like through the API.

mod common;

use aurcache_common::api::package::{ExtendedPackage, PackageSource, UploadedBuild};
use aurcache_common::build_state::BuildState;
use aurcache_db::testing::package;
use common::{serve_api, test_api};
use rocket::http::{ContentType, Status};

const BOUNDARY: &str = "upload-test-boundary";

/// Build logs go somewhere disposable, the same place for every test in this
/// process.
///
/// One place for all of them, because the server reads it from the process's
/// environment: a log is keyed by pkgbase and build number alone, so tests
/// running side by side keep their logs apart by each uploading a pkgbase of
/// its own.
fn quiet_logs() {
    static LOGS: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        // SAFETY: set once, before any test reads it, and never changed.
        unsafe { std::env::set_var("AURCACHE_BUILD_LOG_PATH", &dir) };
        dir
    });
}

/// A `.pkg.tar.zst` holding only a `.PKGINFO`.
fn package_file(pkgname: &str, pkgbase: &str, version: &str, arch: &str) -> Vec<u8> {
    let pkginfo =
        format!("pkgname = {pkgname}\npkgbase = {pkgbase}\npkgver = {version}\narch = {arch}\n");
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_path(".PKGINFO").unwrap();
        header.set_size(pkginfo.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, pkginfo.as_bytes()).unwrap();
        builder.finish().unwrap();
    }
    zstd::stream::encode_all(tar_bytes.as_slice(), 0).unwrap()
}

/// A multipart body carrying `files`, each a `(filename, contents)` part.
fn multipart(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (filename, contents) in files {
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
    let body = multipart(&[(
        "demo-1.0-1-x86_64.pkg.tar.zst".to_string(),
        package_file("demo", "demo", "1.0-1", "x86_64"),
    )]);

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

#[tokio::test]
async fn a_refused_upload_says_why() {
    quiet_logs();
    let api = test_api("test", std::convert::identity).await;

    let no_package = api
        .client
        .post("/api/package/absent/upload")
        .header(form_data())
        .body(multipart(&[(
            "absent-1.0-1-x86_64.pkg.tar.zst".to_string(),
            package_file("absent", "absent", "1.0-1", "x86_64"),
        )]))
        .dispatch()
        .await;
    assert_eq!(no_package.status(), Status::NotFound);
    let missing = api.client.get("/api/package/absent").dispatch().await;
    assert_eq!(missing.status(), Status::NotFound, "nothing was created");

    package("refused").insert(&api.db).await;
    let wrong_name = api
        .client
        .post("/api/package/refused/upload")
        .header(form_data())
        .body(multipart(&[(
            "other-1.0-1-x86_64.pkg.tar.zst".to_string(),
            package_file("other", "other", "1.0-1", "x86_64"),
        )]))
        .dispatch()
        .await;
    assert_eq!(wrong_name.status(), Status::BadRequest);
    let reason = wrong_name.into_string().await.unwrap();
    assert!(
        reason.contains("not among the packages refused's"),
        "{reason}"
    );

    let traversal = api
        .client
        .post("/api/package/refused/upload")
        .header(form_data())
        .body(multipart(&[(
            "../refused-1.0-1-x86_64.pkg.tar.zst".to_string(),
            package_file("refused", "refused", "1.0-1", "x86_64"),
        )]))
        .dispatch()
        .await;
    assert_eq!(traversal.status(), Status::BadRequest);

    let not_multipart = api
        .client
        .post("/api/package/refused/upload")
        .header(ContentType::Binary)
        .body(package_file("refused", "refused", "1.0-1", "x86_64"))
        .dispatch()
        .await;
    assert_eq!(not_multipart.status(), Status::BadRequest);

    let large_signature = api
        .client
        .post("/api/package/refused/upload")
        .header(form_data())
        .body(multipart(&[
            (
                "refused-1.0-1-x86_64.pkg.tar.zst".to_string(),
                package_file("refused", "refused", "1.0-1", "x86_64"),
            ),
            (
                "refused-1.0-1-x86_64.pkg.tar.zst.sig".to_string(),
                vec![0x89; 17 * 1024],
            ),
        ]))
        .dispatch()
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

/// A refusal reaches a client over a real connection as its reason, even
/// while the client is still sending: refused by name part-way through the
/// body, and before any of it is read. The in-process client hands the body
/// over whole, so only a socket shows a server that stops reading early, and
/// only a body streamed from disk, as `aurcli` sends it, is still being sent
/// when the answer comes.
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

/// A client that waits for `100 Continue` before sending the body is told to
/// go on -- Rocket reads the start of every body before routing it -- and so
/// is sending by the time an upload is refused: it reads the refusal once it
/// has sent the body, as any other client does, rather than a broken
/// connection.
#[tokio::test]
async fn a_client_waiting_to_continue_reads_the_refusal() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    quiet_logs();
    let api = serve_api("test").await;
    let address = api
        .url
        .strip_prefix("http://")
        .and_then(|rest| rest.strip_suffix("/api"))
        .unwrap()
        .to_string();
    let body = vec![b'-'; 16 << 20];
    let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
    let head = format!(
        "POST /api/package/absent/upload HTTP/1.1\r\nHost: {address}\r\n\
         Content-Type: multipart/form-data; boundary={BOUNDARY}\r\n\
         Content-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();

    let deadline = std::time::Duration::from_secs(60);
    let mut answer = Vec::new();
    let mut chunk = [0; 4096];
    let continued = tokio::time::timeout(deadline, async {
        while !answer.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk).await.unwrap() {
                0 => break,
                n => answer.extend_from_slice(&chunk[..n]),
            }
        }
    })
    .await;
    let interim = String::from_utf8_lossy(&answer).to_string();
    assert!(continued.is_ok(), "no answer to the head: {interim}");
    assert!(interim.starts_with("HTTP/1.1 100"), "{interim}");

    stream
        .write_all(&body)
        .await
        .expect("the whole body is taken");
    let mut rest = Vec::new();
    tokio::time::timeout(deadline, stream.read_to_end(&mut rest))
        .await
        .expect("the server answers")
        .unwrap();
    let answer = String::from_utf8_lossy(&rest);
    assert!(answer.contains("HTTP/1.1 404"), "{answer}");
}
