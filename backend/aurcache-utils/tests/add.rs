use std::sync::{LazyLock, Mutex, MutexGuard};

use aurcache_db::migration::Migrator;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::{Dependencies, Packages};
use aurcache_db::{builds, dependencies, packages};
use aurcache_types::builder::{Action, BuildStates};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Database, DatabaseConnection, EntityTrait, PaginatorTrait,
    QueryFilter, Set,
};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

use aurcache_utils::package::add::package_add;

// -----------------------------------------------------------------------
// Test helpers
// -----------------------------------------------------------------------

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    old_rpc: Option<String>,
}

impl EnvGuard {
    fn new() -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_rpc = std::env::var("AUR_RPC_URL").ok();
        EnvGuard {
            _lock: lock,
            old_rpc,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.old_rpc {
            Some(v) => unsafe { std::env::set_var("AUR_RPC_URL", v) },
            None => unsafe { std::env::remove_var("AUR_RPC_URL") },
        }
    }
}

struct TestEnv {
    db: DatabaseConnection,
    _rx: tokio::sync::broadcast::Receiver<Action>,
    server: MockServer,
    _guard: EnvGuard,
}

async fn setup_env() -> TestEnv {
    let server = MockServer::start().await;
    let base_url = server.uri();

    let guard = EnvGuard::new();
    unsafe {
        std::env::set_var("AUR_RPC_URL", format!("{base_url}/rpc/v5"));
    }

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
        _guard: guard,
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

async fn add_pkg_via_rpc(env: &TestEnv, name: &str) -> anyhow::Result<String> {
    let (tx, _) = tokio::sync::broadcast::channel(100);
    package_add(
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
    assert_eq!(pkg.directly_requested, 1, "directly_requested should be 1");
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

    let result = add_pkg_via_rpc(&env, "parent-pkg").await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "parent-pkg");

    let child = Packages::find()
        .filter(packages::Column::Pkgbase.eq("child-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("child package should exist");
    assert_eq!(
        child.directly_requested, 0,
        "child is not directly requested"
    );

    let parent = Packages::find()
        .filter(packages::Column::Pkgbase.eq("parent-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("parent package should exist");
    assert_eq!(
        parent.directly_requested, 1,
        "parent should be directly requested"
    );

    let dep = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(parent.id))
        .filter(dependencies::Column::DependeeId.eq(child.id))
        .one(&env.db)
        .await
        .unwrap()
        .expect("dependency row should exist");
    assert_eq!(dep.platforms, "x86_64");

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

    let result = add_pkg_via_rpc(&env, "parent-pkg").await;
    assert!(result.is_ok());

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

    let result = add_pkg_via_rpc(&env, "build-tool").await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "build-tool");

    let dep_pkg = Packages::find()
        .filter(packages::Column::Pkgbase.eq("make-env-pkg"))
        .one(&env.db)
        .await
        .unwrap()
        .expect("make-env-pkg should exist");
    assert_eq!(dep_pkg.directly_requested, 0);

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

    assert_eq!(parent1.directly_requested, 1);
    assert_eq!(parent2.directly_requested, 1);
    assert_eq!(child.directly_requested, 0);
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
    assert_eq!(pkg.directly_requested, 1);
    assert_eq!(pkg.status, BuildStates::ENQUEUED_BUILD);

    let dep_count = Dependencies::find().count(&env.db).await.unwrap();
    assert_eq!(dep_count, 0, "no dependency rows for system packages");
}
