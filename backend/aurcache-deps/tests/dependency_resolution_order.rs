//! The stages of [`AurClient::resolve_dependencies`], in order.
//!
//! Resolution walks three sources -- the official repositories, the packages
//! AURCache tracks, and the AUR (by name, then by `provides`) -- and stops at
//! the first that answers. These tests pin that order, and pin the two things
//! each stage is allowed to conclude: what it resolves to, and what it costs.
//! The cost half matters because nearly every package depends on names that
//! live in `core`/`extra`/`multilib`, and resolving those over the network
//! first spent a request per package on an answer already sitting in a file.

use std::collections::HashSet;
use std::fs::{self, File};
use std::path::Path;

use aurcache_deps::{AurClient, Dependency, DependencyResolution, MatchKind, SatisfyIndex};
use flate2::Compression;
use flate2::write::GzEncoder;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// No package base is favoured over another: what every caller but a
/// re-resolve passes.
fn no_preference() -> HashSet<String> {
    HashSet::new()
}

/// Write one official repository database holding a `desc` entry per package.
fn write_official_db(
    cache_dir: &Path,
    repo_name: &str,
    packages: &[(&str, Option<&str>, &[&str])],
) {
    fs::create_dir_all(cache_dir).unwrap();
    let file = File::create(cache_dir.join(format!("{repo_name}.db.tar.gz"))).unwrap();
    let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::default()));

    for (pkg_name, version, provides) in packages {
        let mut desc = format!("%NAME%\n{pkg_name}\n\n%BASE%\n{pkg_name}\n\n");
        if let Some(version) = version {
            desc.push_str(&format!("%VERSION%\n{version}\n\n"));
        }
        if !provides.is_empty() {
            desc.push_str("%PROVIDES%\n");
            for entry in *provides {
                desc.push_str(entry);
                desc.push('\n');
            }
            desc.push('\n');
        }

        let mut header = tar::Header::new_gnu();
        header
            .set_path(format!("{pkg_name}-{}-1/desc", version.unwrap_or("1.0")))
            .unwrap();
        header.set_size(desc.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, desc.as_bytes()).unwrap();
    }
    builder.finish().unwrap();
}

struct Env {
    _tmp: tempfile::TempDir,
    client: AurClient,
}

/// A client whose RPC points at `rpc_url` and whose official repositories hold
/// `published` -- in `extra`, with `core` and `multilib` present but empty.
///
/// Present and empty rather than absent: resolution refuses to guess when it
/// cannot read the databases, so leaving them out would fail the resolve
/// rather than fall through to the AUR. No mirrorlist is written either, since
/// a cache this fresh is never refreshed.
async fn env_for(rpc_url: &str, published: &[(&str, Option<&str>, &[&str])]) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("official-cache");
    write_official_db(&cache_dir, "extra", published);
    write_official_db(&cache_dir, "core", &[]);
    write_official_db(&cache_dir, "multilib", &[]);

    let client =
        AurClient::with_urls_and_paths(rpc_url, tmp.path().join("no-such-mirrorlist"), cache_dir);
    // What the server does at startup. A cache this fresh is read rather than
    // fetched, so no mirror is needed for it.
    client
        .official
        .refresh()
        .await
        .expect("a warm cache needs no mirror");
    Env { _tmp: tmp, client }
}

/// Stage 2: a package the caller's database tracks resolves to it, without a
/// single request to the AUR.
#[tokio::test]
async fn a_tracked_package_resolves_locally_and_costs_no_rpc_call() {
    let server = MockServer::start().await;
    let env = env_for(&format!("{}/rpc/v5", server.uri()), &[]).await;

    let mut tracked = SatisfyIndex::new();
    tracked.insert("mydep", "mydep-git", MatchKind::Provides, None);

    let resolved = env
        .client
        .resolve_dependencies(
            &[Dependency::unversioned("mydep")],
            &tracked,
            &no_preference(),
        )
        .await
        .unwrap();

    assert_eq!(
        resolved.get("mydep"),
        Some(&DependencyResolution::Local {
            pkgbase: "mydep-git".to_string()
        })
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// An existing edge outranks a better-ranked newcomer.
///
/// Ranking alone would pick `mydep`, which carries the name, over `mydep-git`,
/// which only provides it. Preferring what the package already depends on is
/// what lets someone repoint an edge by hand and have it survive: every edge
/// is recomputed from the declared names on each update, so without this the
/// ranking would take the choice back at once.
#[tokio::test]
async fn a_preferred_package_beats_a_better_ranked_one() {
    let server = MockServer::start().await;
    let env = env_for(&format!("{}/rpc/v5", server.uri()), &[]).await;

    let mut tracked = SatisfyIndex::new();
    tracked.insert("mydep", "mydep", MatchKind::Name, None);
    tracked.insert("mydep", "mydep-git", MatchKind::Provides, None);

    let preferred = HashSet::from(["mydep-git".to_string()]);
    let resolved = env
        .client
        .resolve_dependencies(&[Dependency::unversioned("mydep")], &tracked, &preferred)
        .await
        .unwrap();

    assert_eq!(
        resolved.get("mydep"),
        Some(&DependencyResolution::Local {
            pkgbase: "mydep-git".to_string()
        })
    );
}

/// Without the preference the same index resolves the other way, which is what
/// makes the test above about the preference and not about the index.
#[tokio::test]
async fn ranking_decides_when_nothing_is_preferred() {
    let server = MockServer::start().await;
    let env = env_for(&format!("{}/rpc/v5", server.uri()), &[]).await;

    let mut tracked = SatisfyIndex::new();
    tracked.insert("mydep", "mydep", MatchKind::Name, None);
    tracked.insert("mydep", "mydep-git", MatchKind::Provides, None);

    let resolved = env
        .client
        .resolve_dependencies(
            &[Dependency::unversioned("mydep")],
            &tracked,
            &no_preference(),
        )
        .await
        .unwrap();

    assert_eq!(
        resolved.get("mydep"),
        Some(&DependencyResolution::Local {
            pkgbase: "mydep".to_string()
        })
    );
}

/// A preference is a tie-break among the packages that answer to the name, not
/// a way to keep one that no longer does. A package that stopped providing the
/// name is dropped like any other, and resolution falls through to whatever
/// still claims it.
#[tokio::test]
async fn a_preferred_package_that_no_longer_provides_the_name_is_dropped() {
    let server = MockServer::start().await;
    let env = env_for(&format!("{}/rpc/v5", server.uri()), &[]).await;

    let mut tracked = SatisfyIndex::new();
    tracked.insert("mydep", "mydep", MatchKind::Name, None);

    let preferred = HashSet::from(["mydep-git".to_string()]);
    let resolved = env
        .client
        .resolve_dependencies(&[Dependency::unversioned("mydep")], &tracked, &preferred)
        .await
        .unwrap();

    assert_eq!(
        resolved.get("mydep"),
        Some(&DependencyResolution::Local {
            pkgbase: "mydep".to_string()
        })
    );
}

/// The preference reorders stage 2 only. The official repositories are still
/// asked first, so an edge cannot be pinned to a tracked package for a name
/// the repositories carry.
#[tokio::test]
async fn a_preference_does_not_outrank_the_official_repositories() {
    let server = MockServer::start().await;
    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("git", Some("2.52.0-1"), &[])],
    )
    .await;

    let mut tracked = SatisfyIndex::new();
    tracked.insert("git", "git-git", MatchKind::Provides, None);

    let preferred = HashSet::from(["git-git".to_string()]);
    let resolved = env
        .client
        .resolve_dependencies(&[Dependency::unversioned("git")], &tracked, &preferred)
        .await
        .unwrap();

    assert_eq!(resolved.get("git"), Some(&DependencyResolution::Available));
}

/// Stage 1 wins over stage 2, and how a candidate claims the name does not
/// enter into it.
///
/// The reported bug: `git-git` declares `provides=('git')`, so once it was
/// tracked it captured every dependency on `git` -- including over the `git`
/// in `extra`, which carries the name. Resolving a name to a package is what
/// makes that package tracked, so one bad answer used to become permanent.
#[tokio::test]
async fn the_official_repositories_beat_a_tracked_package_that_provides_the_name() {
    let server = MockServer::start().await;
    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("git", Some("2.52.0-1"), &[])],
    )
    .await;

    let mut tracked = SatisfyIndex::new();
    tracked.insert("git", "git-git", MatchKind::Provides, None);

    let resolved = env
        .client
        .resolve_dependencies(
            &[Dependency::unversioned("git")],
            &tracked,
            &no_preference(),
        )
        .await
        .unwrap();

    assert_eq!(
        resolved.get("git"),
        Some(&DependencyResolution::Available),
        "a tracked provider must not capture a name the repositories carry"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// Stage 1: a dependency the official repositories publish is answered from
/// disk, and the RPC is never contacted.
#[tokio::test]
async fn a_dependency_in_the_official_repositories_costs_no_rpc_call() {
    let server = MockServer::start().await;
    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("mydep", Some("1.0"), &[])],
    )
    .await;

    let resolved = env
        .client
        .resolve_dependencies(
            &[Dependency::unversioned("mydep")],
            &SatisfyIndex::new(),
            &no_preference(),
        )
        .await
        .unwrap();

    assert_eq!(
        resolved.get("mydep"),
        Some(&DependencyResolution::Available)
    );
    let seen = server.received_requests().await.unwrap();
    assert!(
        seen.is_empty(),
        "the AUR was contacted for a dependency already in the repositories: {:?}",
        seen.iter().map(|r| r.url.to_string()).collect::<Vec<_>>()
    );
}

/// The same when the name is only reachable through `%PROVIDES%`, which is how
/// most virtual dependencies resolve.
#[tokio::test]
async fn a_provided_dependency_also_costs_no_rpc_call() {
    let server = MockServer::start().await;
    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("myprovider", Some("1.0"), &["mydep=1.2.3"])],
    )
    .await;

    let resolved = env
        .client
        .resolve_dependencies(
            &[Dependency::unversioned("mydep")],
            &SatisfyIndex::new(),
            &no_preference(),
        )
        .await
        .unwrap();

    assert_eq!(
        resolved.get("mydep"),
        Some(&DependencyResolution::Available)
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// Stage 3: a mix still asks the AUR, but only about the name the repositories
/// could not answer -- and in one request, not one per dependency.
#[tokio::test]
async fn only_the_unresolved_names_reach_the_aur() {
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

    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("known", Some("1.0"), &[])],
    )
    .await;

    let resolved = env
        .client
        .resolve_dependencies(
            &[
                Dependency::unversioned("known"),
                Dependency::unversioned("stranger"),
            ],
            &SatisfyIndex::new(),
            &no_preference(),
        )
        .await
        .unwrap();

    assert_eq!(
        resolved.get("known"),
        Some(&DependencyResolution::Available)
    );
    assert_eq!(
        resolved.get("stranger"),
        Some(&DependencyResolution::Aur {
            pkgbase: "stranger".to_string()
        })
    );

    let urls: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.to_string())
        .collect();
    assert_eq!(urls.len(), 1, "expected one batched request, got {urls:?}");
    assert!(!urls[0].contains("known"), "{urls:?}");
}

/// Stage 4, and the end of the line: a name nothing anywhere provides is
/// reported, not dropped.
#[tokio::test]
async fn a_name_nothing_provides_is_reported_as_unresolved() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"type":"multiinfo","resultcount":0,"results":[],"version":5}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let env = env_for(&format!("{}/rpc/v5", server.uri()), &[]).await;

    let resolved = env
        .client
        .resolve_dependencies(
            &[Dependency::unversioned("libjpeg6")],
            &SatisfyIndex::new(),
            &no_preference(),
        )
        .await
        .unwrap();

    assert!(resolved.get("libjpeg6").is_none());
    assert_eq!(resolved.unresolved, vec!["libjpeg6".to_string()]);
}

/// An unread repository is not an empty one. Until the refresh has succeeded,
/// resolution has no basis for saying a name is absent, and saying it anyway is
/// what sends ordinary `core` names off to be built from the AUR.
#[tokio::test]
async fn resolution_fails_while_the_official_repositories_are_unread() {
    let tmp = tempfile::tempdir().unwrap();
    let client = AurClient::with_urls_and_paths(
        "http://unused.invalid/rpc/v5",
        tmp.path().join("no-such-mirrorlist"),
        tmp.path().join("never-read"),
    );

    let result = client
        .resolve_dependencies(
            &[Dependency::unversioned("glibc")],
            &SatisfyIndex::new(),
            &no_preference(),
        )
        .await;
    assert!(
        result.is_err(),
        "an unread repository must not read as 'not found'"
    );
}

/// Duplicates cost nothing: a name appearing in both `depends` and
/// `makedepends` is resolved once.
#[tokio::test]
async fn a_repeated_dependency_is_resolved_once() {
    let server = MockServer::start().await;
    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("mydep", Some("1.0"), &[])],
    )
    .await;

    let resolved = env
        .client
        .resolve_dependencies(
            &[
                Dependency::unversioned("mydep"),
                Dependency::unversioned("mydep"),
            ],
            &SatisfyIndex::new(),
            &no_preference(),
        )
        .await
        .unwrap();

    assert_eq!(resolved.found.len(), 1);
}
