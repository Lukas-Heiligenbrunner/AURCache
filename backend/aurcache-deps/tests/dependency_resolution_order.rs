//! The repositories are consulted before the AUR RPC.
//!
//! Nearly every package depends on names that live in `core`/`extra`/`multilib`
//! or in AURCache's own repository. Resolving those against the RPC first and
//! then discovering them on disk spent a request per package for an answer we
//! already had, so these tests pin the order by counting the requests the RPC
//! actually receives.

use std::fs::{self, File};
use std::path::Path;

use aurcache_deps::{AurClient, DependencyResolution};
use flate2::Compression;
use flate2::write::GzEncoder;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Write a minimal `repo.db.tar.gz` holding one package's `desc` entry.
///
/// Only `%NAME%` and `%PROVIDES%` are read by the dependency check, so the
/// fixture carries just those rather than a full pacman database.
fn write_repo_db(dir: &Path, pkg_name: &str, provides: &[&str]) {
    fs::create_dir_all(dir).unwrap();
    let mut desc = format!("%NAME%\n{pkg_name}\n\n");
    if !provides.is_empty() {
        desc.push_str("%PROVIDES%\n");
        for entry in provides {
            desc.push_str(entry);
            desc.push('\n');
        }
        desc.push('\n');
    }

    let file = File::create(dir.join("repo.db.tar.gz")).unwrap();
    let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::default()));
    let mut header = tar::Header::new_gnu();
    header.set_path(format!("{pkg_name}-1.0-1/desc")).unwrap();
    header.set_size(desc.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append(&header, desc.as_bytes()).unwrap();
    builder.finish().unwrap();
}

/// A client whose RPC points at `rpc_url`, with empty official-repo config so
/// that check answers `false` without reaching the network.
fn client_for(rpc_url: &str, repo_root: &Path, tmp: &Path) -> AurClient {
    AurClient::with_urls_and_paths(
        rpc_url,
        repo_root,
        // A mirrorlist that does not exist: `official_dependency_exists`
        // swallows the error as "not found", which is what we want when the
        // point of the test is the *local* repository.
        tmp.join("no-such-mirrorlist"),
        tmp.join("official-cache"),
    )
}

/// The headline: a dependency already in AURCache's own repository is resolved
/// from disk, and the RPC is never contacted.
#[tokio::test]
async fn a_dependency_in_the_local_repo_costs_no_rpc_call() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    write_repo_db(&repo_root.join("x86_64"), "mydep", &[]);

    // No routes mounted: any request that arrives is both a failure of the
    // behaviour under test and visible in `received_requests`.
    let server = MockServer::start().await;

    let client = client_for(&format!("{}/rpc/v5", server.uri()), &repo_root, tmp.path());
    let resolved = client.resolve_dependencies(&["mydep"]).await.unwrap();

    assert!(matches!(
        resolved.get("mydep"),
        Some(DependencyResolution::Official)
    ));
    let seen = server.received_requests().await.unwrap();
    assert!(
        seen.is_empty(),
        "the AUR was contacted for a dependency already in the repository: {:?}",
        seen.iter().map(|r| r.url.to_string()).collect::<Vec<_>>()
    );
}

/// The same when the name is only reachable through `%PROVIDES%`, which is how
/// most virtual dependencies resolve.
#[tokio::test]
async fn a_provided_dependency_also_costs_no_rpc_call() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    write_repo_db(&repo_root.join("x86_64"), "myprovider", &["mydep=1.2.3"]);

    let server = MockServer::start().await;
    let client = client_for(&format!("{}/rpc/v5", server.uri()), &repo_root, tmp.path());
    let resolved = client.resolve_dependencies(&["mydep"]).await.unwrap();

    assert!(matches!(
        resolved.get("mydep"),
        Some(DependencyResolution::Official)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// A mix still asks the AUR, but only about the name the repositories could not
/// answer -- and in one request, not one per dependency.
#[tokio::test]
async fn only_the_unresolved_names_reach_the_aur() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    write_repo_db(&repo_root.join("x86_64"), "known", &[]);

    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"type":"multiinfo","resultcount":1,"results":[
                 {"Name":"stranger","PackageBase":"stranger","Version":"1.0-1"}
               ],"version":5}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let client = client_for(&format!("{}/rpc/v5", server.uri()), &repo_root, tmp.path());
    let resolved = client
        .resolve_dependencies(&["known", "stranger"])
        .await
        .unwrap();

    assert!(matches!(
        resolved.get("known"),
        Some(DependencyResolution::Official)
    ));
    assert!(matches!(
        resolved.get("stranger"),
        Some(DependencyResolution::Aur { pkgbase }) if pkgbase == "stranger"
    ));

    let seen = server.received_requests().await.unwrap();
    assert_eq!(seen.len(), 1, "expected exactly one batched RPC request");
    let url = seen[0].url.to_string();
    assert!(
        url.contains("stranger"),
        "the unresolved name was not asked: {url}"
    );
    assert!(
        !url.contains("known"),
        "a name the repository already answered was still sent to the AUR: {url}"
    );
}
