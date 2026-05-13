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

#[tokio::test]
async fn backfill_creates_dependency_links() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "parent-pkg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "results": [{
                "Name": "parent-pkg",
                "PackageBase": "parent-pkg",
                "Depends": ["child-pkg>=1.0"],
                "MakeDepends": []
            }]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/rpc/v5/info"))
        .and(query_param("arg[]", "child-pkg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "multiinfo",
            "results": [{
                "Name": "child-pkg",
                "PackageBase": "child-pkg",
                "Depends": [],
                "MakeDepends": []
            }]
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
        directly_requested: Set(1),
        current_version: Set(None),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&db)
    .await
    .unwrap();

    let client = AurClient::with_aur_url(mock_server.uri());
    backfill_dependencies(&client, &db).await.unwrap();

    let child = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("child-pkg"))
        .one(&db)
        .await
        .unwrap()
        .expect("child-pkg should have been inserted by backfill");
    assert_eq!(
        child.directly_requested, 0,
        "placeholder dep must have directly_requested=0"
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
                {"Name": "libaegis", "PackageBase": "libaegis", "Depends": [], "MakeDepends": []},
                {"Name": "simsimd", "PackageBase": "simsimd", "Depends": [], "MakeDepends": []},
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
            "results": [{
                "Name": "turso",
                "PackageBase": "turso",
                "Depends": ["libaegis", "simsimd"],
                "MakeDepends": []
            }]
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
            "results": [{
                "Name": "libaegis",
                "PackageBase": "libaegis",
                "Depends": [],
                "MakeDepends": []
            }]
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
            "results": [{
                "Name": "simsimd",
                "PackageBase": "simsimd",
                "Depends": [],
                "MakeDepends": []
            }]
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
        directly_requested: Set(1),
        current_version: Set(None),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(&db)
    .await
    .unwrap();

    let client = AurClient::with_aur_url(mock_server.uri());
    backfill_dependencies(&client, &db).await.unwrap();

    // libaegis inserted as placeholder dep
    let libaegis = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("libaegis"))
        .one(&db)
        .await
        .unwrap()
        .expect("libaegis should have been inserted by backfill");
    assert_eq!(libaegis.directly_requested, 0);
    assert_eq!(libaegis.status, 3);

    // simsimd inserted as placeholder dep
    let simsimd = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("simsimd"))
        .one(&db)
        .await
        .unwrap()
        .expect("simsimd should have been inserted by backfill");
    assert_eq!(simsimd.directly_requested, 0);
    assert_eq!(simsimd.status, 3);

    // turso unchanged
    let turso = packages::Entity::find()
        .filter(packages::Column::Pkgbase.eq("turso"))
        .one(&db)
        .await
        .unwrap()
        .expect("turso should exist");
    assert_eq!(turso.directly_requested, 1);

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
