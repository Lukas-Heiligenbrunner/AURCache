use aurcache_db::migration::m20260508_000000_dependency_resolution_combined::backfill_dependencies;
use aurcache_db::{
    dependencies,
    migration::Migrator,
    packages::{self, SourceType},
};
use aurcache_deps::AurClient;
use sea_orm::{ActiveModelTrait, ColumnTrait, Database, EntityTrait, QueryFilter, Set};
use sea_orm_migration::MigratorTrait;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

async fn mock_official_search_fallback(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/packages/search/json/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [],
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn backfill_creates_dependency_links() {
    let mock_server = MockServer::start().await;

    let make_pkg = |name: &str, deps: Vec<&str>| -> serde_json::Value {
        serde_json::json!({
            "Name": name,
            "Version": "1.0-1",
            "PackageBase": name,
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
            "Depends": deps,
            "MakeDepends": [],
            "OptDepends": null,
            "CheckDepends": null,
            "Conflicts": null,
            "Provides": null,
            "Replaces": null,
            "Groups": null,
            "License": null,
            "Keywords": null,
        })
    };

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "parent-pkg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "results": [make_pkg("parent-pkg", vec!["child-pkg>=1.0"])]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "child-pkg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "results": [make_pkg("child-pkg", vec![])]
        })))
        .mount(&mock_server)
        .await;

    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    packages::ActiveModel {
        name: Set("parent-pkg".to_string()),
        pkgbase: Set("parent-pkg".to_string()),
        status: Set(3),
        out_of_date: Set(0),
        upstream_version: Set(None),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"parent-pkg"}"#.to_string()),
        directly_requested: Set(true),
        current_version: Set(None),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&db)
    .await
    .unwrap();

    mock_official_search_fallback(&mock_server).await;
    let client = AurClient::with_urls(
        format!("{}/rpc/v5", mock_server.uri()),
        format!("{}/packages/search/json/", mock_server.uri()),
    );
    backfill_dependencies(&client, &db).await.unwrap();

    let child = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("child-pkg"))
        .one(&db)
        .await
        .unwrap()
        .expect("child-pkg should have been inserted by backfill");
    assert!(
        !child.directly_requested,
        "placeholder dep must have directly_requested=false"
    );

    let parent = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("parent-pkg"))
        .one(&db)
        .await
        .unwrap()
        .expect("parent-pkg should exist");

    let links = dependencies::Entity::find()
        .filter(dependencies::Column::DependentId.eq(parent.id))
        .filter(dependencies::Column::DependeeId.eq(child.id))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(
        links.len(),
        1,
        "parent-pkg -> child-pkg dependency link must exist"
    );
}

#[tokio::test]
async fn backfill_multi_dep_package() {
    let mock_server = MockServer::start().await;

    let make_pkg = |name: &str, deps: Vec<&str>| -> serde_json::Value {
        serde_json::json!({
            "Name": name,
            "Version": "1.0-1",
            "PackageBase": name,
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
            "Depends": deps,
            "MakeDepends": [],
            "OptDepends": null,
            "CheckDepends": null,
            "Conflicts": null,
            "Provides": null,
            "Replaces": null,
            "Groups": null,
            "License": null,
            "Keywords": null,
        })
    };

    // Wiremock: first-match-wins. Register the most specific (batch) mock FIRST.
    // Batch resolve_bases(["libaegis", "simsimd"]) -> multiinfo with both
    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "libaegis"))
        .and(query_param("arg[]", "simsimd"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "resultcount": 2,
            "results": [
                make_pkg("libaegis", vec![]),
                make_pkg("simsimd", vec![]),
            ]
        })))
        .mount(&mock_server)
        .await;

    // deps_of("turso") -> depends on libaegis and simsimd
    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "turso"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "resultcount": 1,
            "results": [make_pkg("turso", vec!["libaegis", "simsimd"])]
        })))
        .mount(&mock_server)
        .await;

    // deps_of("libaegis") -> leaf
    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "libaegis"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "resultcount": 1,
            "results": [make_pkg("libaegis", vec![])]
        })))
        .mount(&mock_server)
        .await;

    // deps_of("simsimd") -> leaf
    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "simsimd"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "resultcount": 1,
            "results": [make_pkg("simsimd", vec![])]
        })))
        .mount(&mock_server)
        .await;

    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    // Pre-insert turso (as if it existed pre-migration)
    packages::ActiveModel {
        name: Set("turso".to_string()),
        pkgbase: Set("turso".to_string()),
        status: Set(3),
        out_of_date: Set(0),
        upstream_version: Set(None),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"turso"}"#.to_string()),
        directly_requested: Set(true),
        current_version: Set(None),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&db)
    .await
    .unwrap();

    mock_official_search_fallback(&mock_server).await;
    let client = AurClient::with_urls(
        format!("{}/rpc/v5", mock_server.uri()),
        format!("{}/packages/search/json/", mock_server.uri()),
    );
    backfill_dependencies(&client, &db).await.unwrap();

    // libaegis inserted as placeholder dep
    let libaegis = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("libaegis"))
        .one(&db)
        .await
        .unwrap()
        .expect("libaegis should have been inserted by backfill");
    assert!(!libaegis.directly_requested);
    assert_eq!(libaegis.status, 3);

    // simsimd inserted as placeholder dep
    let simsimd = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("simsimd"))
        .one(&db)
        .await
        .unwrap()
        .expect("simsimd should have been inserted by backfill");
    assert!(!simsimd.directly_requested);
    assert_eq!(simsimd.status, 3);

    // turso unchanged
    let turso = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("turso"))
        .one(&db)
        .await
        .unwrap()
        .expect("turso should exist");
    assert!(turso.directly_requested);

    // Dependency links: turso -> libaegis and turso -> simsimd
    let turso_to_libaegis = dependencies::Entity::find()
        .filter(dependencies::Column::DependentId.eq(turso.id))
        .filter(dependencies::Column::DependeeId.eq(libaegis.id))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(
        turso_to_libaegis.len(),
        1,
        "turso -> libaegis dep link must exist"
    );

    let turso_to_simsimd = dependencies::Entity::find()
        .filter(dependencies::Column::DependentId.eq(turso.id))
        .filter(dependencies::Column::DependeeId.eq(simsimd.id))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(
        turso_to_simsimd.len(),
        1,
        "turso -> simsimd dep link must exist"
    );
}

#[tokio::test]
async fn backfill_resolves_provider_dependencies() {
    let mock_server = MockServer::start().await;

    let make_pkg = |name: &str, deps: Vec<&str>| -> serde_json::Value {
        serde_json::json!({
            "Name": name,
            "Version": "1.0-1",
            "PackageBase": name,
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
            "Depends": deps,
            "MakeDepends": [],
            "OptDepends": null,
            "CheckDepends": null,
            "Conflicts": null,
            "Provides": null,
            "Replaces": null,
            "Groups": null,
            "License": null,
            "Keywords": null,
        })
    };

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "parent-pkg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "results": [make_pkg("parent-pkg", vec!["virtual-dep>=2.0"])]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "virtual-dep"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "results": []
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/rpc/v5/search/virtual-dep"))
        .and(query_param("by", "provides"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "search",
            "resultcount": 1,
            "results": [make_pkg("provider-pkg", vec![])]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "provider-pkg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "results": [make_pkg("provider-pkg", vec![])]
        })))
        .mount(&mock_server)
        .await;

    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    packages::ActiveModel {
        name: Set("parent-pkg".to_string()),
        pkgbase: Set("parent-pkg".to_string()),
        status: Set(3),
        out_of_date: Set(0),
        upstream_version: Set(None),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"parent-pkg"}"#.to_string()),
        directly_requested: Set(true),
        current_version: Set(None),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&db)
    .await
    .unwrap();

    mock_official_search_fallback(&mock_server).await;
    let client = AurClient::with_urls(
        format!("{}/rpc/v5", mock_server.uri()),
        format!("{}/packages/search/json/", mock_server.uri()),
    );
    backfill_dependencies(&client, &db).await.unwrap();

    let parent = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("parent-pkg"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let provider = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("provider-pkg"))
        .one(&db)
        .await
        .unwrap()
        .expect("provider package should have been inserted");

    let link = dependencies::Entity::find()
        .filter(dependencies::Column::DependentId.eq(parent.id))
        .filter(dependencies::Column::DependeeId.eq(provider.id))
        .one(&db)
        .await
        .unwrap()
        .expect("provider dependency link should exist");
    assert_eq!(link.version_constraint, ">=2.0");
}

#[tokio::test]
async fn backfill_prefers_existing_local_provider() {
    let mock_server = MockServer::start().await;

    let make_pkg = |name: &str, deps: Vec<&str>, provides: Vec<&str>| -> serde_json::Value {
        serde_json::json!({
            "Name": name,
            "Version": "1.0-1",
            "PackageBase": name,
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
            "Depends": deps,
            "MakeDepends": [],
            "OptDepends": null,
            "CheckDepends": null,
            "Conflicts": null,
            "Provides": provides,
            "Replaces": null,
            "Groups": null,
            "License": null,
            "Keywords": null,
        })
    };

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "parent-pkg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "results": [make_pkg("parent-pkg", vec!["virtual-dep>=2.0"], vec![])]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "local-provider"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "results": [make_pkg("local-provider", vec![], vec!["virtual-dep"])]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "virtual-dep"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "results": []
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/rpc/v5/search/virtual-dep"))
        .and(query_param("by", "provides"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "search",
            "resultcount": 1,
            "results": [make_pkg("aur-provider", vec![], vec![])]
        })))
        .mount(&mock_server)
        .await;

    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    packages::ActiveModel {
        name: Set("parent-pkg".to_string()),
        pkgbase: Set("parent-pkg".to_string()),
        status: Set(3),
        out_of_date: Set(0),
        upstream_version: Set(None),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"parent-pkg"}"#.to_string()),
        directly_requested: Set(true),
        current_version: Set(None),
        split_packages: Set(None),
        provides: Set(None),
        ..Default::default()
    }
    .save(&db)
    .await
    .unwrap();

    let local_provider = packages::ActiveModel {
        name: Set("local-provider".to_string()),
        pkgbase: Set("local-provider".to_string()),
        status: Set(3),
        out_of_date: Set(0),
        upstream_version: Set(Some("1.0-1".to_string())),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;--noprogressbar".to_string()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(r#"{"type":"aur","name":"local-provider"}"#.to_string()),
        directly_requested: Set(false),
        current_version: Set(None),
        split_packages: Set(None),
        provides: Set(Some(r#"["virtual-dep"]"#.to_string())),
        ..Default::default()
    }
    .save(&db)
    .await
    .unwrap();

    mock_official_search_fallback(&mock_server).await;
    let client = AurClient::with_urls(
        format!("{}/rpc/v5", mock_server.uri()),
        format!("{}/packages/search/json/", mock_server.uri()),
    );
    backfill_dependencies(&client, &db).await.unwrap();

    let parent = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("parent-pkg"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    let link = dependencies::Entity::find()
        .filter(dependencies::Column::DependentId.eq(parent.id))
        .filter(dependencies::Column::DependeeId.eq(local_provider.id.unwrap()))
        .one(&db)
        .await
        .unwrap()
        .expect("local provider dependency link should exist");
    assert_eq!(link.version_constraint, ">=2.0");

    let aur_provider = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("aur-provider"))
        .one(&db)
        .await
        .unwrap();
    assert!(
        aur_provider.is_none(),
        "local providers should prevent AUR inserts"
    );
}
