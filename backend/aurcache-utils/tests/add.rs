use aurcache_db::migration::Migrator;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::{Dependencies, Packages};
use aurcache_db::{builds, dependencies, packages};
use aurcache_deps::AurClient;
use aurcache_types::builder::{Action, BuildStates};
use aurcache_utils::package::enqueue::enqueue_missing_buildable_packages;
use aurcache_utils::pkg::satisfies_constraint;
use flate2::{Compression, write::GzEncoder};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Database, DatabaseConnection, EntityTrait, PaginatorTrait,
    QueryFilter, Set, TryIntoModel,
};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use std::fs::{self};
use std::path::Path;
use tar::{Builder, Header};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

use aurcache_utils::package::add::package_add_with_client;

// -----------------------------------------------------------------------
// Test helpers
// -----------------------------------------------------------------------

struct TestEnv {
    db: DatabaseConnection,
    _rx: tokio::sync::broadcast::Receiver<Action>,
    server: MockServer,
    client: AurClient,
    _repo_dir: TempDir,
    _official_dir: TempDir,
    _repo_root: std::path::PathBuf,
    official_cache_dir: std::path::PathBuf,
}

async fn setup_env() -> TestEnv {
    let server = MockServer::start().await;
    let base_url = server.uri();
    let repo_dir = tempfile::tempdir().expect("failed to create repo tempdir");
    let official_dir = tempfile::tempdir().expect("failed to create official tempdir");
    let repo_root = repo_dir.path().join("repo");
    let mirrorlist_path = official_dir.path().join("mirrorlist");
    let official_cache_dir = official_dir.path().join("cache");
    fs::create_dir_all(repo_root.join("x86_64")).expect("failed to create repo path");
    fs::write(
        &mirrorlist_path,
        format!("Server = {base_url}/$repo/os/$arch/\n"),
    )
    .expect("failed to write mirrorlist");
    fs::create_dir_all(&official_cache_dir).expect("failed to create official cache dir");
    seed_official_repo_cache_empty(&official_cache_dir);

    let client = AurClient::with_urls_and_paths(
        format!("{base_url}/rpc/v5"),
        format!("{base_url}/packages/search/json/"),
        repo_root.clone(),
        mirrorlist_path,
        official_cache_dir.clone(),
    );

    let db = Database::connect("sqlite::memory:")
        .await
        .expect("failed to create in-memory DB");
    Migrator::up(&db, None)
        .await
        .expect("failed to run migrations");

    let (_, rx) = tokio::sync::broadcast::channel(100);

    TestEnv {
        db,
        _rx: rx,
        server,
        client,
        _repo_dir: repo_dir,
        _official_dir: official_dir,
        _repo_root: repo_root,
        official_cache_dir,
    }
}

async fn mock_rpc_info(server: &MockServer, pkgbase: &str, result: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", pkgbase))
        .respond_with(ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![result])))
        .mount(server)
        .await;
}

async fn mock_official_search(server: &MockServer, query: &str, results: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/packages/search/json/"))
        .and(query_param("q", query))
        .respond_with(ResponseTemplate::new(200).set_body_json(results))
        .mount(server)
        .await;
}

fn seed_official_repo_cache_empty(cache_dir: &Path) {
    for repo_name in ["core", "extra", "multilib"] {
        fs::write(
            cache_dir.join(format!("{repo_name}.db.tar.gz")),
            repo_archive_bytes(&[]),
        )
        .expect("failed to seed official repo cache");
    }
}

async fn mock_official_repo_db(
    server: &MockServer,
    repo_name: &str,
    packages: Vec<(&str, Vec<&str>)>,
) {
    let bytes = repo_archive_bytes(
        &packages
            .into_iter()
            .map(|(name, provides)| (name.to_string(), provides))
            .collect::<Vec<_>>(),
    );
    Mock::given(method("GET"))
        .and(path(&format!("/{repo_name}/os/x86_64/{repo_name}.db")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        .mount(server)
        .await;
}

fn repo_archive_bytes(packages: &[(String, Vec<&str>)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let encoder = GzEncoder::new(&mut bytes, Compression::default());
    let mut builder = Builder::new(encoder);
    for (name, provides) in packages {
        let dir_name = format!("{name}-1.0.0-1");
        append_tar_dir(&mut builder, &dir_name);
        append_tar_file(
            &mut builder,
            &format!("{dir_name}/desc"),
            &repo_desc(name, provides),
        );
    }
    builder.finish().expect("failed to finalize repo archive");
    drop(builder);
    bytes
}

fn append_tar_dir<W: std::io::Write>(builder: &mut Builder<W>, path: &str) {
    let mut header = Header::new_gnu();
    header.set_path(path).expect("invalid dir path");
    header.set_entry_type(tar::EntryType::Directory);
    header.set_mode(0o755);
    header.set_size(0);
    header.set_cksum();
    builder
        .append(&header, std::io::empty())
        .expect("failed to append dir");
}

fn append_tar_file<W: std::io::Write>(builder: &mut Builder<W>, path: &str, content: &str) {
    let mut header = Header::new_gnu();
    header.set_path(path).expect("invalid file path");
    header.set_mode(0o644);
    header.set_size(content.len() as u64);
    header.set_cksum();
    builder
        .append(&header, content.as_bytes())
        .expect("failed to append file");
}

fn repo_desc(name: &str, provides: &[&str]) -> String {
    let mut desc = format!(
        "%FILENAME%\n{name}-1.0.0-1-x86_64.pkg.tar.zst\n\n\
         %NAME%\n{name}\n\n\
         %BASE%\n{name}\n\n\
         %VERSION%\n1.0.0-1\n\n\
         %DESC%\n{name}\n\n\
         %CSIZE%\n1\n\n\
         %ISIZE%\n1\n\n\
         %SHA256SUM%\n0000000000000000000000000000000000000000000000000000000000000000\n\n\
         %ARCH%\nx86_64\n\n\
         %BUILDDATE%\n1\n\n\
         %PACKAGER%\nTest <test@example.com>\n\n"
    );
    if !provides.is_empty() {
        desc.push_str("%PROVIDES%\n");
        desc.push_str(&provides.join("\n"));
        desc.push_str("\n\n");
    }
    desc
}

fn rpc_deps_json(
    name: &str,
    pkgbase: &str,
    depends: &[&str],
    make_depends: &[&str],
    version: &str,
) -> serde_json::Value {
    json!({
        "Name": name,
        "Version": version,
        "PackageBase": pkgbase,
        "PackageBaseID": 0,
        "ID": 0,
        "NumVotes": 0,
        "Popularity": 0.0,
        "FirstSubmitted": 0,
        "LastModified": 0,
        "URLPath": null,
        "Description": null,
        "Maintainer": null,
        "URL": null,
        "OutOfDate": null,
        "Depends": depends,
        "MakeDepends": make_depends,
        "OptDepends": null,
        "CheckDepends": null,
        "Conflicts": null,
        "Provides": null,
        "Replaces": null,
        "Groups": null,
        "License": null,
        "Keywords": null,
    })
}

fn multiinfo_json(results: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "type": "multiinfo",
        "resultcount": results.len(),
        "results": results,
    })
}

fn official_search_json(results: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "results": results,
    })
}

async fn add_pkg_via_rpc(env: &TestEnv, name: &str) -> anyhow::Result<String> {
    let (tx, _) = tokio::sync::broadcast::channel(100);
    package_add_with_client(
        &env.client,
        &env.db,
        &tx,
        None,
        None,
        SourceData::Aur {
            name: name.to_string(),
        },
    )
    .await
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn scenario_a_no_aur_deps() {
    let env = setup_env().await;

    mock_rpc_info(
        &env.server,
        "no-deps-pkg",
        rpc_deps_json("no-deps-pkg", "no-deps-pkg", &[], &[], "1.0.0"),
    )
    .await;

    let result = add_pkg_via_rpc(&env, "no-deps-pkg").await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "no-deps-pkg");

    let pkg = Packages::find()
        .filter(packages::Column::Pkgbase.eq("no-deps-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("package should exist");
    assert!(pkg.directly_requested, "directly_requested should be true");
    assert_eq!(pkg.status, BuildStates::ENQUEUED_BUILD);
    assert_eq!(pkg.upstream_version, Some("1.0.0".to_string()));

    let dep_count = Dependencies::find().count(&env.db).await.unwrap();
    assert_eq!(dep_count, 0, "dependencies table should be empty");
}

#[tokio::test]
async fn scenario_b_one_aur_dep() {
    let env = setup_env().await;

    mock_rpc_info(
        &env.server,
        "parent-pkg",
        rpc_deps_json("parent-pkg", "parent-pkg", &["child-pkg"], &[], "2.0.0"),
    )
    .await;

    mock_rpc_info(
        &env.server,
        "child-pkg",
        rpc_deps_json("child-pkg", "child-pkg", &[], &[], "1.0.0"),
    )
    .await;
    mock_official_search(&env.server, "child-pkg", official_search_json(vec![])).await;
    mock_official_search(&env.server, "child-pkg", official_search_json(vec![])).await;

    let result = add_pkg_via_rpc(&env, "parent-pkg").await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(result.unwrap(), "parent-pkg");

    let child = Packages::find()
        .filter(packages::Column::Pkgbase.eq("child-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("child package should exist");
    assert!(!child.directly_requested, "child is not directly requested");

    let parent = Packages::find()
        .filter(packages::Column::Pkgbase.eq("parent-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("parent package should exist");
    assert!(
        parent.directly_requested,
        "parent should be directly requested"
    );

    let dep = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(parent.id))
        .filter(dependencies::Column::DependeeId.eq(child.id))
        .one(&env.db)
        .await
        .unwrap()
        .expect("dependency row should exist");
    assert_eq!(dep.version_constraint, "");

    let parent_builds = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(parent.id))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(
        parent_builds, 0,
        "parent should not have a build triggered yet"
    );
}

#[tokio::test]
async fn scenario_c_cascade_after_dep_build() {
    let env = setup_env().await;

    mock_rpc_info(
        &env.server,
        "parent-pkg",
        rpc_deps_json("parent-pkg", "parent-pkg", &["child-pkg"], &[], "2.0.0"),
    )
    .await;
    mock_rpc_info(
        &env.server,
        "child-pkg",
        rpc_deps_json("child-pkg", "child-pkg", &[], &[], "1.0.0"),
    )
    .await;
    mock_official_search(&env.server, "child-pkg", official_search_json(vec![])).await;

    let result = add_pkg_via_rpc(&env, "parent-pkg").await;
    assert!(result.is_ok(), "{result:?}");

    let child = Packages::find()
        .filter(packages::Column::Pkgbase.eq("child-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .unwrap();

    let child_build = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(child.id))
        .one(&env.db)
        .await
        .unwrap()
        .expect("child should have a build record from trigger_leaf_builds");
    let mut build_active: builds::ActiveModel = child_build.into();
    build_active.status = Set(Some(BuildStates::SUCCESSFUL_BUILD));
    build_active.save(&env.db).await.unwrap();

    let parent = Packages::find()
        .filter(packages::Column::Pkgbase.eq("parent-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .unwrap();

    let dep_count = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(parent.id))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(dep_count, 1, "parent should still have one dependency");

    let child_build = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(child.id))
        .one(&env.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        child_build.status,
        Some(BuildStates::SUCCESSFUL_BUILD),
        "child build should be successful"
    );

    let parent_build_count = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(parent.id))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(parent_build_count, 0, "parent should not be built yet");
}

#[tokio::test]
async fn scenario_d_make_dep_only() {
    let env = setup_env().await;

    mock_rpc_info(
        &env.server,
        "build-tool",
        rpc_deps_json("build-tool", "build-tool", &[], &["make-env-pkg"], "3.0.0"),
    )
    .await;
    mock_rpc_info(
        &env.server,
        "make-env-pkg",
        rpc_deps_json("make-env-pkg", "make-env-pkg", &[], &[], "1.0.0"),
    )
    .await;
    mock_official_search(&env.server, "make-env-pkg", official_search_json(vec![])).await;

    let result = add_pkg_via_rpc(&env, "build-tool").await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "build-tool");

    let dep_pkg = Packages::find()
        .filter(packages::Column::Pkgbase.eq("make-env-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("make-env-pkg should exist");
    assert!(!dep_pkg.directly_requested);

    let tool = Packages::find()
        .filter(packages::Column::Pkgbase.eq("build-tool"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("build-tool should exist");

    let dep = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(tool.id))
        .one(&env.db)
        .await
        .unwrap()
        .expect("dependency row should exist");
    assert_eq!(
        dep.dependee_id, dep_pkg.id,
        "build-tool should depend on make-env-pkg"
    );
}

#[tokio::test]
async fn scenario_e_shared_dep_no_duplicate() {
    let env = setup_env().await;

    mock_rpc_info(
        &env.server,
        "parent-1",
        rpc_deps_json("parent-1", "parent-1", &["child"], &[], "1.0.0"),
    )
    .await;
    mock_rpc_info(
        &env.server,
        "child",
        rpc_deps_json("child", "child", &[], &[], "1.0.0"),
    )
    .await;
    mock_official_search(&env.server, "child", official_search_json(vec![])).await;

    let result1 = add_pkg_via_rpc(&env, "parent-1").await;
    assert!(result1.is_ok());

    mock_rpc_info(
        &env.server,
        "parent-2",
        rpc_deps_json("parent-2", "parent-2", &["child"], &[], "2.0.0"),
    )
    .await;

    let result2 = add_pkg_via_rpc(&env, "parent-2").await;
    assert!(result2.is_ok());

    let child_pkgs = Packages::find()
        .filter(packages::Column::Pkgbase.eq("child"))
        .all(&env.db)
        .await
        .unwrap();
    assert_eq!(child_pkgs.len(), 1, "child should exist exactly once");

    let parent1 = Packages::find()
        .filter(packages::Column::Pkgbase.eq("parent-1"))
        .one(&env.db)
        .await
        .unwrap()
        .unwrap();
    let parent2 = Packages::find()
        .filter(packages::Column::Pkgbase.eq("parent-2"))
        .one(&env.db)
        .await
        .unwrap()
        .unwrap();
    let child = &child_pkgs[0];

    let dep1 = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(parent1.id))
        .filter(dependencies::Column::DependeeId.eq(child.id))
        .one(&env.db)
        .await
        .unwrap()
        .expect("parent-1 -> child dep should exist");
    let dep2 = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(parent2.id))
        .filter(dependencies::Column::DependeeId.eq(child.id))
        .one(&env.db)
        .await
        .unwrap()
        .expect("parent-2 -> child dep should exist");

    assert_eq!(dep1.dependee_id, child.id);
    assert_eq!(dep2.dependee_id, child.id);

    assert!(parent1.directly_requested);
    assert!(parent2.directly_requested);
    assert!(!child.directly_requested);
}

#[tokio::test]
async fn scenario_f_system_deps_only() {
    let env = setup_env().await;

    mock_rpc_info(
        &env.server,
        "my-pkg",
        rpc_deps_json("my-pkg", "my-pkg", &["glibc>=2.35"], &[], "1.0.0"),
    )
    .await;

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "glibc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![])))
        .mount(&env.server)
        .await;
    fs::remove_file(env.official_cache_dir.join("core.db.tar.gz")).unwrap();
    mock_official_repo_db(&env.server, "core", vec![("glibc", vec![])]).await;

    let result = add_pkg_via_rpc(&env, "my-pkg").await;
    assert!(
        result.is_ok(),
        "add should succeed despite system-only deps: {result:?}"
    );
    assert_eq!(result.unwrap(), "my-pkg");

    let pkg = Packages::find()
        .filter(packages::Column::Pkgbase.eq("my-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("package should exist");
    assert!(pkg.directly_requested);
    assert_eq!(pkg.status, BuildStates::ENQUEUED_BUILD);

    let dep_count = Dependencies::find().count(&env.db).await.unwrap();
    assert_eq!(dep_count, 0, "no dependency rows for system packages");
}

#[tokio::test]
async fn scenario_g_split_package_constraints_are_merged_per_pkgbase() {
    let env = setup_env().await;

    mock_rpc_info(
        &env.server,
        "parent-pkg",
        rpc_deps_json(
            "parent-pkg",
            "parent-pkg",
            &["shared-base>=2.0", "shared-lib>=3.0"],
            &[],
            "1.0.0",
        ),
    )
    .await;

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "shared-base"))
        .and(query_param("arg[]", "shared-lib"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![
                rpc_deps_json("shared-base", "shared-base", &[], &[], "3.0.0"),
                rpc_deps_json("shared-lib", "shared-base", &[], &[], "3.0.0"),
            ])),
        )
        .mount(&env.server)
        .await;
    mock_official_search(&env.server, "shared-base", official_search_json(vec![])).await;
    mock_official_search(&env.server, "shared-lib", official_search_json(vec![])).await;

    mock_rpc_info(
        &env.server,
        "shared-base",
        rpc_deps_json("shared-base", "shared-base", &[], &[], "3.0.0"),
    )
    .await;

    let result = add_pkg_via_rpc(&env, "parent-pkg").await;
    assert!(result.is_ok(), "{result:?}");

    let parent = Packages::find()
        .filter(packages::Column::Pkgbase.eq("parent-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("parent package should exist");
    let shared = Packages::find()
        .filter(packages::Column::Pkgbase.eq("shared-base"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("shared dependency package should exist");

    let deps = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(parent.id))
        .filter(dependencies::Column::DependeeId.eq(shared.id))
        .all(&env.db)
        .await
        .unwrap();
    assert_eq!(
        deps.len(),
        1,
        "split package deps should collapse to one row"
    );

    let constraint = &deps[0].version_constraint;
    assert!(
        satisfies_constraint("3.0.0", constraint),
        "merged constraint should accept the stricter version"
    );
    assert!(
        !satisfies_constraint("2.5.0", constraint),
        "merged constraint should reject versions that only satisfy the weaker bound"
    );
}

#[tokio::test]
async fn scenario_h_provider_dependency_resolves_to_aur_package() {
    let env = setup_env().await;

    mock_rpc_info(
        &env.server,
        "parent-pkg",
        rpc_deps_json(
            "parent-pkg",
            "parent-pkg",
            &["virtual-dep>=2.0"],
            &[],
            "1.0.0",
        ),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "virtual-dep"))
        .respond_with(ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![])))
        .mount(&env.server)
        .await;
    mock_official_search(&env.server, "virtual-dep", official_search_json(vec![])).await;
    Mock::given(method("GET"))
        .and(path("/rpc/v5/search/virtual-dep"))
        .and(query_param("by", "provides"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "search",
            "resultcount": 2,
            "results": [
                rpc_deps_json("virtual-provider", "virtual-provider", &[], &[], "2.0.0"),
                rpc_deps_json("zzz-provider", "zzz-provider", &[], &[], "1.0.0")
            ]
        })))
        .mount(&env.server)
        .await;
    mock_rpc_info(
        &env.server,
        "virtual-provider",
        rpc_deps_json("virtual-provider", "virtual-provider", &[], &[], "2.0.0"),
    )
    .await;

    let result = add_pkg_via_rpc(&env, "parent-pkg").await;
    assert!(result.is_ok(), "{result:?}");

    let parent = Packages::find()
        .filter(packages::Column::Pkgbase.eq("parent-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("parent package should exist");
    let provider = Packages::find()
        .filter(packages::Column::Pkgbase.eq("virtual-provider"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("provider package should exist");

    let dep = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(parent.id))
        .filter(dependencies::Column::DependeeId.eq(provider.id))
        .one(&env.db)
        .await
        .unwrap()
        .expect("provider dependency should be linked");
    assert_eq!(dep.version_constraint, ">=2.0");
}

#[tokio::test]
async fn scenario_i_official_provider_prevents_aur_dependency_addition() {
    let env = setup_env().await;

    mock_rpc_info(
        &env.server,
        "parent-pkg",
        rpc_deps_json("parent-pkg", "parent-pkg", &["virtual-dep"], &[], "1.0.0"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "virtual-dep"))
        .respond_with(ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![])))
        .mount(&env.server)
        .await;
    fs::remove_file(env.official_cache_dir.join("core.db.tar.gz")).unwrap();
    mock_official_repo_db(&env.server, "core", vec![("libglvnd", vec!["virtual-dep"])]).await;

    let result = add_pkg_via_rpc(&env, "parent-pkg").await;
    assert!(result.is_ok());

    let parent = Packages::find()
        .filter(packages::Column::Pkgbase.eq("parent-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("parent package should exist");

    let dep_count = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(parent.id))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(
        dep_count, 0,
        "official providers should not create AUR deps"
    );

    let provider_count = Packages::find()
        .filter(packages::Column::Pkgbase.eq("virtual-provider"))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(
        provider_count, 0,
        "no AUR provider package should be inserted"
    );
}

#[tokio::test]
async fn scenario_j_local_queued_provider_prevents_aur_dependency_addition() {
    let env = setup_env().await;
    packages::ActiveModel {
        name: Set("local-provider".to_string()),
        pkgbase: Set("local-provider".to_string()),
        status: Set(BuildStates::ENQUEUED_BUILD),
        out_of_date: Set(0),
        upstream_version: Set(Some("1.0.0".to_string())),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(packages::SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"local-provider"}"#.to_string()),
        directly_requested: Set(false),
        current_version: Set(Some("1.0.0".to_string())),
        split_packages: Set(None),
        provides: Set(Some(r#"["virtual-dep"]"#.to_string())),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap();

    mock_rpc_info(
        &env.server,
        "parent-pkg",
        rpc_deps_json("parent-pkg", "parent-pkg", &["virtual-dep"], &[], "1.0.0"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "virtual-dep"))
        .respond_with(ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![])))
        .mount(&env.server)
        .await;
    mock_official_search(&env.server, "virtual-dep", official_search_json(vec![])).await;
    Mock::given(method("GET"))
        .and(path("/rpc/v5/search/virtual-dep"))
        .and(query_param("by", "provides"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "search",
            "resultcount": 1,
            "results": [rpc_deps_json("aur-provider", "aur-provider", &[], &[], "1.0.0")]
        })))
        .mount(&env.server)
        .await;

    let result = add_pkg_via_rpc(&env, "parent-pkg").await;
    assert!(result.is_ok(), "{result:?}");

    let parent = Packages::find()
        .filter(packages::Column::Pkgbase.eq("parent-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("parent package should exist");
    let local_provider = Packages::find()
        .filter(packages::Column::Pkgbase.eq("local-provider"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("local provider should exist");
    let link = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(parent.id))
        .filter(dependencies::Column::DependeeId.eq(local_provider.id))
        .one(&env.db)
        .await
        .unwrap();
    assert!(
        link.is_some(),
        "queued local providers should be linked as dependencies"
    );

    let aur_provider_count = Packages::find()
        .filter(packages::Column::Pkgbase.eq("aur-provider"))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(
        aur_provider_count, 0,
        "local providers should win over AUR providers"
    );
}

#[tokio::test]
async fn scenario_k_self_resolved_dependency_is_ignored() {
    let env = setup_env().await;

    mock_rpc_info(
        &env.server,
        "self-base",
        rpc_deps_json("self-base", "self-base", &["self-split>=1.0"], &[], "1.0.0"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "self-split"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                "self-split",
                "self-base",
                &[],
                &[],
                "1.0.0",
            )])),
        )
        .mount(&env.server)
        .await;
    mock_official_search(&env.server, "self-split", official_search_json(vec![])).await;

    let result = add_pkg_via_rpc(&env, "self-base").await;
    assert!(result.is_ok(), "{result:?}");

    let pkg = Packages::find()
        .filter(packages::Column::Pkgbase.eq("self-base"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("package should exist");

    let dep_count = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(pkg.id))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(dep_count, 0, "self-resolved dependencies should be ignored");

    let pkg_count = Packages::find()
        .filter(packages::Column::Pkgbase.eq("self-base"))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(
        pkg_count, 1,
        "self-resolved dependencies should not duplicate the package"
    );
}

#[tokio::test]
async fn scenario_h_queue_missing_buildable_packages_after_migration() {
    let env = setup_env().await;
    let (tx, _) = tokio::sync::broadcast::channel(100);

    let root = packages::ActiveModel {
        name: Set("root".to_string()),
        pkgbase: Set("root".to_string()),
        status: Set(BuildStates::SUCCESSFUL_BUILD),
        out_of_date: Set(0),
        upstream_version: Set(Some("1.0.0".to_string())),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(packages::SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"root"}"#.to_string()),
        directly_requested: Set(true),
        current_version: Set(Some("1.0.0".to_string())),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap()
    .try_into_model()
    .unwrap();

    let mid = packages::ActiveModel {
        name: Set("mid".to_string()),
        pkgbase: Set("mid".to_string()),
        status: Set(BuildStates::FAILED_BUILD),
        out_of_date: Set(0),
        upstream_version: Set(Some("1.0.0".to_string())),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(packages::SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"mid"}"#.to_string()),
        directly_requested: Set(false),
        current_version: Set(None),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap()
    .try_into_model()
    .unwrap();

    let leaf = packages::ActiveModel {
        name: Set("leaf".to_string()),
        pkgbase: Set("leaf".to_string()),
        status: Set(BuildStates::FAILED_BUILD),
        out_of_date: Set(0),
        upstream_version: Set(Some("1.0.0".to_string())),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(packages::SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"leaf"}"#.to_string()),
        directly_requested: Set(false),
        current_version: Set(None),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap()
    .try_into_model()
    .unwrap();

    dependencies::ActiveModel {
        dependent_id: Set(root.id),
        dependee_id: Set(mid.id),
        version_constraint: Set("".to_string()),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap();

    dependencies::ActiveModel {
        dependent_id: Set(mid.id),
        dependee_id: Set(leaf.id),
        version_constraint: Set("".to_string()),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap();

    let queued = enqueue_missing_buildable_packages(&env.db, &tx)
        .await
        .unwrap();

    assert_eq!(queued, 1, "only leaf dependency packages should be queued");

    let leaf_builds = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(leaf.id))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(leaf_builds, 1, "leaf dependency should get a build");

    let mid_builds = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(mid.id))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(mid_builds, 0, "non-leaf dependency should wait for cascade");
}

#[tokio::test]
async fn scenario_i_queue_non_leaf_packages_when_dependencies_are_already_built() {
    let env = setup_env().await;
    let (tx, _) = tokio::sync::broadcast::channel(100);

    let root = packages::ActiveModel {
        name: Set("root".to_string()),
        pkgbase: Set("root".to_string()),
        status: Set(BuildStates::FAILED_BUILD),
        out_of_date: Set(0),
        upstream_version: Set(Some("1.0.0".to_string())),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(packages::SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"root"}"#.to_string()),
        directly_requested: Set(true),
        current_version: Set(None),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap()
    .try_into_model()
    .unwrap();

    let mid = packages::ActiveModel {
        name: Set("mid".to_string()),
        pkgbase: Set("mid".to_string()),
        status: Set(BuildStates::FAILED_BUILD),
        out_of_date: Set(0),
        upstream_version: Set(Some("1.0.0".to_string())),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(packages::SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"mid"}"#.to_string()),
        directly_requested: Set(false),
        current_version: Set(None),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap()
    .try_into_model()
    .unwrap();

    let leaf = packages::ActiveModel {
        name: Set("leaf".to_string()),
        pkgbase: Set("leaf".to_string()),
        status: Set(BuildStates::SUCCESSFUL_BUILD),
        out_of_date: Set(0),
        upstream_version: Set(Some("1.0.0".to_string())),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(packages::SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"leaf"}"#.to_string()),
        directly_requested: Set(false),
        current_version: Set(Some("1.0.0".to_string())),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap()
    .try_into_model()
    .unwrap();

    dependencies::ActiveModel {
        dependent_id: Set(root.id),
        dependee_id: Set(mid.id),
        version_constraint: Set("".to_string()),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap();

    dependencies::ActiveModel {
        dependent_id: Set(mid.id),
        dependee_id: Set(leaf.id),
        version_constraint: Set(">=1.0".to_string()),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap();

    builds::ActiveModel {
        pkg_id: Set(leaf.id),
        output: Set(None),
        status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
        start_time: Set(Some(1)),
        end_time: Set(Some(2)),
        platform: Set("x86_64".to_string()),
        version: Set("1.0.0".to_string()),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap();

    let queued = enqueue_missing_buildable_packages(&env.db, &tx)
        .await
        .unwrap();

    assert_eq!(
        queued, 1,
        "startup should queue packages whose dependencies are already built"
    );

    let root_builds = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(root.id))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(root_builds, 0, "root should wait until mid is built");

    let mid_builds = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(mid.id))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(
        mid_builds, 1,
        "non-leaf package should be queued when its deps are satisfied"
    );
}

#[tokio::test]
async fn scenario_j_queue_only_platforms_with_satisfied_dependencies() {
    let env = setup_env().await;
    let (tx, _) = tokio::sync::broadcast::channel(100);

    let root = packages::ActiveModel {
        name: Set("root".to_string()),
        pkgbase: Set("root".to_string()),
        status: Set(BuildStates::FAILED_BUILD),
        out_of_date: Set(0),
        upstream_version: Set(Some("1.0.0".to_string())),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64;aarch64".to_string()),
        source_type: Set(packages::SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"root"}"#.to_string()),
        directly_requested: Set(true),
        current_version: Set(None),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap()
    .try_into_model()
    .unwrap();

    let leaf = packages::ActiveModel {
        name: Set("leaf".to_string()),
        pkgbase: Set("leaf".to_string()),
        status: Set(BuildStates::SUCCESSFUL_BUILD),
        out_of_date: Set(0),
        upstream_version: Set(Some("1.0.0".to_string())),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64;aarch64".to_string()),
        source_type: Set(packages::SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"leaf"}"#.to_string()),
        directly_requested: Set(false),
        current_version: Set(Some("1.0.0".to_string())),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap()
    .try_into_model()
    .unwrap();

    dependencies::ActiveModel {
        dependent_id: Set(root.id),
        dependee_id: Set(leaf.id),
        version_constraint: Set(">=1.0".to_string()),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap();

    builds::ActiveModel {
        pkg_id: Set(leaf.id),
        output: Set(None),
        status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
        start_time: Set(Some(1)),
        end_time: Set(Some(2)),
        platform: Set("x86_64".to_string()),
        version: Set("1.0.0".to_string()),
        ..Default::default()
    }
    .save(&env.db)
    .await
    .unwrap();

    let queued = enqueue_missing_buildable_packages(&env.db, &tx)
        .await
        .unwrap();

    assert_eq!(
        queued, 2,
        "startup should queue the ready dependent platform and any other ready leaf builds"
    );

    let root_x86_builds = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(root.id))
        .filter(builds::Column::Platform.eq("x86_64"))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(root_x86_builds, 1, "x86_64 should be queued");

    let root_aarch64_builds = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(root.id))
        .filter(builds::Column::Platform.eq("aarch64"))
        .count(&env.db)
        .await
        .unwrap();
    assert_eq!(
        root_aarch64_builds, 0,
        "aarch64 should wait until its dependency is built for that platform"
    );
}
