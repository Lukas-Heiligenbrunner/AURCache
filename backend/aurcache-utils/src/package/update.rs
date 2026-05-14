use crate::package::add::ensure_aur_package_exists_recursive;
use anyhow::{anyhow, bail};
use async_recursion::async_recursion;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::package_update_activity::PackageUpdateActivity;
use aurcache_db::activities::ActivityType;
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use aurcache_db::{builds, dependencies, packages};
use aurcache_deps::{AurClient, PkgDeps};
use aurcache_types::builder::{Action, BuildStates};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait, TryIntoModel,
};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;
use tracing::info;

/// Updates all outdated packages in the database.
pub async fn package_update_all_outdated(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
) -> anyhow::Result<Vec<i32>> {
    let txn = db.begin().await?;

    let pkg_models: Vec<packages::Model> = Packages::find()
        .filter(packages::Column::OutOfDate.eq(1))
        .all(&txn)
        .await?;
    let activity_log = ActivityLog::new(db.clone());

    let mut ids_total = vec![];
    for pkg in &pkg_models {
        if pkg.status == BuildStates::SUCCESSFUL_BUILD {
            let mut ids = package_update(db, pkg.to_owned(), false, tx).await?;
            activity_log
                .add(
                    PackageUpdateActivity {
                        package: pkg.name.clone(),
                        forced: false,
                    },
                    ActivityType::UpdatePackage,
                    Some("Server".to_string()),
                )
                .await?;
            ids_total.append(&mut ids);
        } else {
            info!(
                "Package auto update was not triggered for package {} because of prev. build status: {}",
                pkg.name, pkg.status
            );
        }
    }
    Ok(ids_total)
}

pub async fn package_update(
    db: &DatabaseConnection,
    pkg_model: packages::Model,
    force: bool,
    tx: &Sender<Action>,
) -> anyhow::Result<Vec<i32>> {
    let client = AurClient::new();
    package_update_with_client(&client, db, pkg_model, force, tx).await
}

#[async_recursion]
pub async fn package_update_with_client(
    client: &AurClient,
    db: &DatabaseConnection,
    pkg_model: packages::Model,
    force: bool,
    tx: &Sender<Action>,
) -> anyhow::Result<Vec<i32>> {
    let source_data = packages::SourceData::from_str(pkg_model.source_data.as_str())?;

    let update_context = match source_data {
        packages::SourceData::Aur { .. } => {
            let (upstream_version, deps) = client
                .deps_of_with_version(pkg_model.pkgbase.as_str())
                .await
                .map_err(|e| anyhow!("Failed to resolve latest package metadata: {e}"))?;
            let ready_platforms =
                sync_aur_dependency_graph(client, db, &pkg_model, &deps, tx).await?;
            UpdateContext {
                upstream_version,
                split_packages: split_packages_json(&pkg_model.pkgbase, &deps.pkgnames)?,
                ready_platforms,
            }
        }
        packages::SourceData::Git { .. } => UpdateContext {
            upstream_version: pkg_model
                .upstream_version
                .clone()
                .ok_or(anyhow!("No latest version in package"))?,
            split_packages: pkg_model.split_packages.clone(),
            ready_platforms: configured_platforms(&pkg_model.platforms),
        },
        packages::SourceData::Upload { .. } => {
            todo!("Get version from zip")
        }
    };

    let latest_build = Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_model.id))
        .order_by_desc(builds::Column::StartTime)
        .one(db)
        .await?;

    if let Some(build) = latest_build
        && !force
        && build.version == update_context.upstream_version
    {
        bail!(
            "Latest build is already up to date (version {})",
            update_context.upstream_version
        );
    }

    let mut pkg_model_active: packages::ActiveModel = pkg_model.clone().into();
    pkg_model_active.status = Set(BuildStates::ENQUEUED_BUILD);
    pkg_model_active.upstream_version = Set(Some(update_context.upstream_version.clone()));
    pkg_model_active.split_packages = Set(update_context.split_packages.clone());
    let txn = db.begin().await?;
    let pkg_active_model = pkg_model_active.save(&txn).await?;
    txn.commit().await?;

    if update_context.ready_platforms.is_empty() {
        return Ok(vec![]);
    }

    let mut build_ids = vec![];
    let pkg_model: packages::Model = pkg_active_model.try_into()?;
    for platform in &update_context.ready_platforms {
        let build_id = update_platform(
            platform,
            pkg_model.clone(),
            update_context.upstream_version.clone(),
            db,
            tx,
        )
        .await?;
        build_ids.push(build_id);
    }

    Ok(build_ids)
}

struct UpdateContext {
    upstream_version: String,
    split_packages: Option<String>,
    ready_platforms: Vec<String>,
}

async fn sync_aur_dependency_graph(
    client: &AurClient,
    db: &DatabaseConnection,
    pkg_model: &packages::Model,
    deps: &PkgDeps,
    tx: &Sender<Action>,
) -> anyhow::Result<Vec<String>> {
    let configured_platforms = configured_platforms(&pkg_model.platforms);
    let dep_constraints = collect_dependency_constraints(deps);
    let dep_constraints_by_pkgbase =
        resolve_dependency_constraints_by_pkgbase(client, &dep_constraints)
            .await
            .map_err(|e| anyhow!("Failed to resolve AUR dependency bases: {e}"))?;

    for dep_pkgbase in dep_constraints_by_pkgbase.keys() {
        if Packages::find()
            .filter(packages::Column::Pkgbase.eq(dep_pkgbase.as_str()))
            .one(db)
            .await?
            .is_none()
        {
            ensure_aur_package_exists_recursive(
                client,
                db,
                dep_pkgbase,
                &pkg_model.platforms,
                &pkg_model.build_flags,
            )
            .await?;
        }
    }

    if dep_constraints_by_pkgbase.is_empty() {
        sync_dependency_rows(
            db,
            pkg_model.id,
            &dep_constraints_by_pkgbase,
            &HashMap::new(),
        )
        .await?;
        return Ok(configured_platforms);
    }

    let dep_packages = Packages::find()
        .filter(
            packages::Column::Pkgbase.is_in(
                dep_constraints_by_pkgbase
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
        )
        .all(db)
        .await?
        .into_iter()
        .map(|pkg| (pkg.pkgbase.clone(), pkg))
        .collect::<HashMap<_, _>>();

    sync_dependency_rows(
        db,
        pkg_model.id,
        &dep_constraints_by_pkgbase,
        &dep_packages,
    )
    .await?;

    let mut ready_platforms = Vec::new();
    for platform in &configured_platforms {
        if dependencies_ready_for_platform(
            client,
            db,
            tx,
            platform,
            &dep_constraints_by_pkgbase,
            &dep_packages,
        )
        .await?
        {
            ready_platforms.push(platform.clone());
        }
    }

    Ok(ready_platforms)
}

fn collect_dependency_constraints(deps: &PkgDeps) -> HashMap<String, String> {
    let mut dep_constraints: HashMap<String, String> = HashMap::new();
    for dep in deps.depends.iter().chain(deps.make_depends.iter()) {
        let (name, constraint) = aurcache_deps::parse_dep(dep);
        dep_constraints
            .entry(name.to_string())
            .and_modify(|existing| {
                *existing = crate::pkg::merge_version_constraints(existing.as_str(), constraint);
            })
            .or_insert_with(|| constraint.to_string());
    }

    dep_constraints
}

async fn resolve_dependency_constraints_by_pkgbase(
    client: &AurClient,
    dep_constraints: &HashMap<String, String>,
) -> Result<HashMap<String, String>, aurcache_deps::Error> {
    if dep_constraints.is_empty() {
        return Ok(HashMap::new());
    }

    let dep_names = dep_constraints
        .keys()
        .map(|name| name.as_str())
        .collect::<Vec<_>>();
    let aur_dep_bases = client.resolve_bases(&dep_names).await?;

    let mut dep_constraints_by_pkgbase: HashMap<String, String> = HashMap::new();
    for (dep_name, dep_pkgbase) in aur_dep_bases {
        let constraint = dep_constraints
            .get(dep_name.as_str())
            .map_or("", String::as_str);

        dep_constraints_by_pkgbase
            .entry(dep_pkgbase)
            .and_modify(|existing| {
                *existing = crate::pkg::merge_version_constraints(existing.as_str(), constraint);
            })
            .or_insert_with(|| constraint.to_string());
    }

    Ok(dep_constraints_by_pkgbase)
}

async fn sync_dependency_rows(
    db: &DatabaseConnection,
    dependent_id: i32,
    dep_constraints_by_pkgbase: &HashMap<String, String>,
    dep_packages: &HashMap<String, packages::Model>,
) -> anyhow::Result<()> {
    let txn = db.begin().await?;
    let desired_dependee_ids = dep_packages
        .keys()
        .filter_map(|pkgbase| dep_packages.get(pkgbase).map(|pkg| pkg.id))
        .collect::<Vec<_>>();

    for existing in Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent_id))
        .all(&txn)
        .await?
    {
        if !desired_dependee_ids.contains(&existing.dependee_id) {
            existing.delete(&txn).await?;
        }
    }

    for (dep_pkgbase, constraint) in dep_constraints_by_pkgbase {
        let Some(dep_pkg) = dep_packages.get(dep_pkgbase) else {
            continue;
        };

        if let Some(existing) = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(dependent_id))
            .filter(dependencies::Column::DependeeId.eq(dep_pkg.id))
            .one(&txn)
            .await?
        {
            let mut active: dependencies::ActiveModel = existing.into();
            active.version_constraint = Set(constraint.clone());
            active.save(&txn).await?;
        } else {
            dependencies::ActiveModel {
                dependent_id: Set(dependent_id),
                dependee_id: Set(dep_pkg.id),
                version_constraint: Set(constraint.clone()),
                ..Default::default()
            }
            .save(&txn)
            .await?;
        }
    }

    txn.commit().await?;
    Ok(())
}

async fn dependency_satisfies_constraint(
    db: &DatabaseConnection,
    dependee_id: i32,
    platform: &str,
    constraint: &str,
) -> anyhow::Result<bool> {
    let Some(build) = Builds::find()
        .select_only()
        .column(builds::Column::Version)
        .filter(builds::Column::PkgId.eq(dependee_id))
        .filter(builds::Column::Platform.eq(platform))
        .filter(builds::Column::Status.eq(Some(BuildStates::SUCCESSFUL_BUILD)))
        .order_by(builds::Column::EndTime, sea_orm::Order::Desc)
        .order_by(builds::Column::StartTime, sea_orm::Order::Desc)
        .into_tuple::<(String,)>()
        .one(db)
        .await?
    else {
        return Ok(false);
    };

    Ok(constraint.is_empty() || crate::pkg::satisfies_constraint(&build.0, constraint))
}

async fn dependencies_ready_for_platform(
    client: &AurClient,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platform: &str,
    dep_constraints_by_pkgbase: &HashMap<String, String>,
    dep_packages: &HashMap<String, packages::Model>,
) -> anyhow::Result<bool> {
    for (dep_pkgbase, constraint) in dep_constraints_by_pkgbase {
        let Some(dep_pkg) = dep_packages.get(dep_pkgbase) else {
            return Ok(false);
        };

        if dependency_satisfies_constraint(db, dep_pkg.id, platform, constraint).await? {
            continue;
        }

        let has_pending_build = Builds::find()
            .filter(builds::Column::PkgId.eq(dep_pkg.id))
            .filter(builds::Column::Platform.eq(platform))
            .filter(builds::Column::Status.is_in(vec![
                Some(BuildStates::ENQUEUED_BUILD),
                Some(BuildStates::ACTIVE_BUILD),
            ]))
            .count(db)
            .await?
            > 0;

        if !has_pending_build {
            package_update_with_client(client, db, dep_pkg.clone(), true, tx).await?;
        }

        return Ok(false);
    }

    Ok(true)
}

fn configured_platforms(platforms: &str) -> Vec<String> {
    platforms
        .split(';')
        .filter(|platform| !platform.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn split_packages_json(pkgbase: &str, pkgnames: &[String]) -> anyhow::Result<Option<String>> {
    if pkgnames.len() > 1 || pkgnames.first().is_some_and(|name| name != pkgbase) {
        Ok(Some(serde_json::to_string(pkgnames)?))
    } else {
        Ok(None)
    }
}

/// Creates a build entry for a package on a specific platform.
pub async fn update_platform(
    platform: &str,
    pkg: packages::Model,
    new_version: String,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
) -> anyhow::Result<i32> {
    let txn = db.begin().await?;
    let start_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let build = builds::ActiveModel {
        pkg_id: Set(pkg.id),
        output: Set(None),
        status: Set(Some(BuildStates::ENQUEUED_BUILD)),
        start_time: Set(Some(start_time)),
        platform: Set(platform.to_string()),
        version: Set(new_version),
        ..Default::default()
    };
    let new_build = build.save(&txn).await?;
    let build_id = new_build.id.clone().unwrap();
    txn.commit().await?;

    let _ = tx.send(Action::Build(
        Box::from(pkg),
        Box::from(new_build.try_into_model()?),
    ));
    Ok(build_id)
}

#[cfg(test)]
mod tests {
    use super::package_update_with_client;
    use aurcache_db::migration::Migrator;
    use aurcache_db::prelude::{Dependencies, Packages};
    use aurcache_db::{builds, dependencies, packages};
    use aurcache_deps::AurClient;
    use aurcache_types::builder::{Action, BuildStates};
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, Database, EntityTrait, PaginatorTrait, QueryFilter, Set,
        TryIntoModel,
    };
    use sea_orm_migration::MigratorTrait;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

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

    #[tokio::test]
    async fn package_update_queues_dependency_builds_before_parent_when_constraints_tighten() {
        let server = MockServer::start().await;
        let client = AurClient::with_aur_url(format!("{}/rpc/v5", server.uri()));
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "parent"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "parent",
                    "parent",
                    &["child>=2.0"],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "child"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "child",
                    "child",
                    &[],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            pkgbase: Set("parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"parent"}"#.to_string()),
            directly_requested: Set(true),
            current_version: Set(Some("1.0.0".to_string())),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            pkgbase: Set("child".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"child"}"#.to_string()),
            directly_requested: Set(false),
            current_version: Set(Some("1.0.0".to_string())),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(child.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set("x86_64".to_string()),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let build_ids = package_update_with_client(&client, &db, parent.clone(), false, &tx)
            .await
            .unwrap();

        assert!(
            build_ids.is_empty(),
            "parent should wait for dependency rebuild"
        );

        let updated_dep = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(parent.id))
            .filter(dependencies::Column::DependeeId.eq(child.id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated_dep.version_constraint, ">=2.0");

        let parent_build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(parent.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(parent_build_count, 0);

        let child_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            child_builds, 2,
            "dependency should get a new rebuild queued"
        );

        let parent_after = Packages::find_by_id(parent.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(parent_after.status, BuildStates::ENQUEUED_BUILD);
        assert_eq!(parent_after.upstream_version.as_deref(), Some("2.0.0"));
    }

    #[tokio::test]
    async fn package_update_does_not_queue_non_leaf_dependency_builds() {
        let server = MockServer::start().await;
        let client = AurClient::with_aur_url(format!("{}/rpc/v5", server.uri()));
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "parent"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "parent",
                    "parent",
                    &["child>=2.0"],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "child"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "child",
                    "child",
                    &["grandchild>=2.0"],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "grandchild"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "grandchild",
                    "grandchild",
                    &[],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            pkgbase: Set("parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"parent"}"#.to_string()),
            directly_requested: Set(true),
            current_version: Set(Some("1.0.0".to_string())),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            pkgbase: Set("child".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"child"}"#.to_string()),
            directly_requested: Set(false),
            current_version: Set(Some("1.0.0".to_string())),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let grandchild = packages::ActiveModel {
            name: Set("grandchild".to_string()),
            pkgbase: Set("grandchild".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"grandchild"}"#.to_string()),
            directly_requested: Set(false),
            current_version: Set(Some("1.0.0".to_string())),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(child.id),
            dependee_id: Set(grandchild.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(child.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set("x86_64".to_string()),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(grandchild.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set("x86_64".to_string()),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let build_ids = package_update_with_client(&client, &db, parent.clone(), false, &tx)
            .await
            .unwrap();

        assert!(
            build_ids.is_empty(),
            "parent should wait for transitive dependency rebuilds"
        );

        let parent_build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(parent.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(parent_build_count, 0);

        let child_build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            child_build_count, 1,
            "non-leaf dependency should not get a new build until its own dependencies are ready"
        );

        let grandchild_build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(grandchild.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            grandchild_build_count, 2,
            "leaf transitive dependency should be queued first"
        );

        let child_after = Packages::find_by_id(child.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child_after.status, BuildStates::ENQUEUED_BUILD);
        assert_eq!(child_after.upstream_version.as_deref(), Some("2.0.0"));
    }

    #[tokio::test]
    async fn force_rebuild_does_not_queue_non_leaf_dependency_builds() {
        let server = MockServer::start().await;
        let client = AurClient::with_aur_url(format!("{}/rpc/v5", server.uri()));
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "parent"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "parent",
                    "parent",
                    &["child>=2.0"],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "child"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "child",
                    "child",
                    &["grandchild>=2.0"],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "grandchild"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "grandchild",
                    "grandchild",
                    &[],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            pkgbase: Set("parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"parent"}"#.to_string()),
            directly_requested: Set(true),
            current_version: Set(Some("1.0.0".to_string())),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            pkgbase: Set("child".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"child"}"#.to_string()),
            directly_requested: Set(false),
            current_version: Set(Some("1.0.0".to_string())),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let grandchild = packages::ActiveModel {
            name: Set("grandchild".to_string()),
            pkgbase: Set("grandchild".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"grandchild"}"#.to_string()),
            directly_requested: Set(false),
            current_version: Set(Some("1.0.0".to_string())),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(child.id),
            dependee_id: Set(grandchild.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(child.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set("x86_64".to_string()),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(grandchild.id),
            output: Set(None),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set("x86_64".to_string()),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let build_ids = package_update_with_client(&client, &db, parent.clone(), true, &tx)
            .await
            .unwrap();

        assert!(
            build_ids.is_empty(),
            "forced rebuild should still wait for transitive dependency rebuilds"
        );

        let parent_build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(parent.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(parent_build_count, 0);

        let child_build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            child_build_count, 1,
            "forced rebuild must not enqueue a non-leaf dependency"
        );

        let grandchild_build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(grandchild.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            grandchild_build_count, 2,
            "forced rebuild should enqueue only the leaf transitive dependency first"
        );
    }
}
