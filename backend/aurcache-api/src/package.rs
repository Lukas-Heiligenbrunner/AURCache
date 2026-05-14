use crate::models::authenticated::Authenticated;
use crate::models::package::{AddPackage, PackagePatchModel, UpdatePackage};
use crate::models::package::{
    AurNotFoundPackage, AurPackage, ExtendedPackageModel, GitPackage, PackageDependencyModel,
    PackageSource, SimplePackageModel,
};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::package_add_activity::PackageAddActivity;
use aurcache_activitylog::package_delete_activity::PackageDeleteActivity;
use aurcache_activitylog::package_update_activity::PackageUpdateActivity;
use aurcache_db::activities::ActivityType;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use aurcache_db::{builds, dependencies, packages};
use aurcache_types::builder::Action;
use aurcache_utils::aur::api::get_package_info;
use aurcache_utils::package::add::package_add;
use aurcache_utils::package::delete::package_delete;
use aurcache_utils::package::live_check::package_remove;
use aurcache_utils::package::update::package_update;
use pacman_mirrors::platforms::Platform;
use rocket::http::Status;
use rocket::response::status::{BadRequest, Custom, NotFound};
use rocket::serde::json::Json;
use rocket::{State, delete, get, patch, post};
use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::prelude::Expr;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Order};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use std::collections::HashMap;
use std::str::FromStr;
use tokio::sync::broadcast::Sender;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(
    package_add_endpoint,
    package_update_entity_endpoint,
    package_update_endpoint,
    package_del,
    package_remove_endpoint,
    package_list,
    get_package
))]
pub struct PackageApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Add new Package"),
    )
)]
#[post("/package", data = "<input>")]
pub async fn package_add_endpoint(
    db: &State<DatabaseConnection>,
    input: Json<AddPackage>,
    tx: &State<Sender<Action>>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<(), BadRequest<String>> {
    let platforms = match input.platforms.clone() {
        None => None,
        Some(v) => Some(
            v.into_iter()
                .map(|s| Platform::from_str(&s).ok())
                .collect::<Option<Vec<Platform>>>()
                .ok_or(BadRequest("Invalid Platform name".to_string()))?,
        ),
    };

    let new_pkg_name = package_add(
        db,
        tx,
        platforms,
        input.build_flags.clone(),
        input.source.clone(),
    )
    .await
    .map_err(|e| BadRequest(e.to_string()))?;

    al.add(
        PackageAddActivity {
            package: new_pkg_name,
        },
        ActivityType::AddPackage,
        a.username,
    )
    .await
    .map_err(|e| BadRequest(e.to_string()))?;
    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Update parts of package"),
    ),
    params(
            ("id", description = "Id of package")
    )
)]
#[patch("/package/<id>", data = "<input>")]
pub async fn package_update_entity_endpoint(
    db: &State<DatabaseConnection>,
    input: Json<PackagePatchModel>,
    id: i32,
    _a: Authenticated,
) -> Result<(), BadRequest<String>> {
    let db = db as &DatabaseConnection;

    let new_name = input.name.clone();

    // Start building the update operation
    let update_pkg = packages::ActiveModel {
        id: Set(id),
        name: new_name.clone().map_or(NotSet, Set),
        pkgbase: new_name.clone().map_or(NotSet, Set),
        status: input.status.map_or(NotSet, Set),
        out_of_date: input.out_of_date.map_or(NotSet, Set),
        upstream_version: NotSet,
        latest_build: input.latest_build.map_or(NotSet, Set),
        build_flags: input
            .build_flags
            .clone()
            .map_or(NotSet, |v| Set(v.join(";"))),
        platforms: input.platforms.clone().map_or(NotSet, |v| Set(v.join(";"))),
        source_type: NotSet,
        source_data: NotSet,
        directly_requested: NotSet,
        current_version: NotSet,
        split_packages: NotSet,
    };

    // Execute the update query
    update_pkg
        .update(db)
        .await
        .map_err(|e| BadRequest(e.to_string()))?;

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Update package to newest AUR version"),
    ),
    params(
            ("id", description = "Id of package")
    )
)]
#[post("/package/<id>/update", data = "<input>")]
pub async fn package_update_endpoint(
    db: &State<DatabaseConnection>,
    id: i32,
    input: Json<UpdatePackage>,
    tx: &State<Sender<Action>>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<Json<Vec<i32>>, BadRequest<String>> {
    let db = db as &DatabaseConnection;

    let pkg_model: packages::Model = Packages::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| BadRequest(e.to_string()))?
        .ok_or(BadRequest("id not found".to_string()))?;

    let pkg_update = package_update(db, pkg_model.clone(), input.force, tx)
        .await
        .map(Json)
        .map_err(|e| BadRequest(e.to_string()))?;

    al.add(
        PackageUpdateActivity {
            package: pkg_model.name,
            forced: input.force,
        },
        ActivityType::UpdatePackage,
        a.username,
    )
    .await
    .map_err(|e| BadRequest(e.to_string()))?;
    Ok(pkg_update)
}

#[utoipa::path(
    responses(
            (status = 200, description = "Delete package"),
    ),
    params(
            ("id", description = "Id of package")
    )
)]
#[delete("/package/<id>")]
pub async fn package_del(
    db: &State<DatabaseConnection>,
    id: i32,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<(), BadRequest<String>> {
    let db = db as &DatabaseConnection;

    // query this before deleting package!
    let pkg = Packages::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| BadRequest(e.to_string()))?
        .ok_or(BadRequest("id not found".to_string()))?;

    package_delete(db, id)
        .await
        .map_err(|e| BadRequest(e.to_string()))?;

    al.add(
        PackageDeleteActivity { package: pkg.name },
        ActivityType::RemovePackage,
        a.username,
    )
    .await
    .map_err(|e| BadRequest(e.to_string()))?;

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Remove direct request flag from package and live-check it"),
    ),
    params(
            ("id", description = "Id of package")
    )
)]
#[post("/package/<id>/remove")]
pub async fn package_remove_endpoint(
    db: &State<DatabaseConnection>,
    id: i32,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<(), BadRequest<String>> {
    let db = db as &DatabaseConnection;

    let pkg = Packages::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| BadRequest(e.to_string()))?
        .ok_or(BadRequest("id not found".to_string()))?;

    package_remove(db, id)
        .await
        .map_err(|e| BadRequest(e.to_string()))?;

    al.add(
        PackageDeleteActivity { package: pkg.name },
        ActivityType::RemovePackage,
        a.username,
    )
    .await
    .map_err(|e| BadRequest(e.to_string()))?;

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "List of all packages", body = [SimplePackageModel]),
    ),
    params(
            ("limit", description = "limit of packages"),
            ("page", description = "page of packages")
    )
)]
#[get("/packages/list?<limit>&<page>")]
pub async fn package_list(
    db: &State<DatabaseConnection>,
    limit: Option<u64>,
    page: Option<u64>,
    _a: Authenticated,
) -> Result<Json<Vec<SimplePackageModel>>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    list_directly_requested_packages(db, limit, page)
        .await
        .map(Json)
        .map_err(|e| NotFound(e.to_string()))
}

async fn list_directly_requested_packages(
    db: &DatabaseConnection,
    limit: Option<u64>,
    page: Option<u64>,
) -> Result<Vec<SimplePackageModel>, sea_orm::DbErr> {
    // correlated subquery: picks the version from builds for the package ordered by most
    // recent timestamp (end_time preferred, fallback to start_time)
    let latest_version_subquery = "(SELECT version \
        FROM builds b \
        WHERE b.pkg_id = packages.id \
        ORDER BY COALESCE(b.end_time, b.start_time) DESC \
        LIMIT 1)";

    let all: Vec<SimplePackageModel> = Packages::find()
        .select_only()
        .column(packages::Column::Name)
        .column(packages::Column::Id)
        .column(packages::Column::Status)
        .column_as(packages::Column::OutOfDate, "outofdate")
        .column_as(packages::Column::UpstreamVersion, "upstream_version")
        .filter(packages::Column::DirectlyRequested.eq(true))
        // wrap the correlated subquery in COALESCE -> fallback to empty string
        .column_as(
            Expr::cust(format!("COALESCE({latest_version_subquery}, '')")),
            "latest_version",
        )
        .order_by(packages::Column::OutOfDate, Order::Desc)
        .order_by(packages::Column::Id, Order::Desc)
        .limit(limit)
        .offset(page.zip(limit).map(|(page, limit)| page * limit))
        .into_model::<SimplePackageModel>()
        .all(db)
        .await?;

    Ok(all)
}

async fn list_package_relations(
    db: &DatabaseConnection,
    pkg_id: i32,
    dep_column: dependencies::Column,
    pkg_column: dependencies::Column,
) -> Result<Vec<PackageDependencyModel>, sea_orm::DbErr> {
    let dependency_links = Dependencies::find()
        .filter(dep_column.eq(pkg_id))
        .order_by_asc(dependencies::Column::Id)
        .all(db)
        .await?;

    if dependency_links.is_empty() {
        return Ok(vec![]);
    }

    let dependees = Packages::find()
        .filter(
            packages::Column::Id.is_in(
                dependency_links
                    .iter()
                    .map(|dep| match pkg_column {
                        dependencies::Column::DependentId => dep.dependent_id,
                        dependencies::Column::DependeeId => dep.dependee_id,
                        _ => unreachable!("package relation queries only support id columns"),
                    })
                    .collect::<Vec<_>>(),
            ),
        )
        .all(db)
        .await?
        .into_iter()
        .map(|pkg| (pkg.id, pkg))
        .collect::<HashMap<_, _>>();

    Ok(dependency_links
        .into_iter()
        .filter_map(|dep| {
            let package_id = match pkg_column {
                dependencies::Column::DependentId => dep.dependent_id,
                dependencies::Column::DependeeId => dep.dependee_id,
                _ => unreachable!("package relation queries only support id columns"),
            };

            dependees
                .get(&package_id)
                .map(|pkg| PackageDependencyModel {
                    id: pkg.id,
                    name: pkg.pkgbase.clone(),
                    version_constraint: dep.version_constraint.clone(),
                })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{list_directly_requested_packages, list_package_relations};
    use aurcache_db::migration::Migrator;
    use aurcache_db::{dependencies, packages};
    use sea_orm::{ActiveModelTrait, Database, Set, TryIntoModel};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn package_list_only_returns_directly_requested_packages() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        packages::ActiveModel {
            name: Set("visible-package".to_string()),
            pkgbase: Set("visible-package".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"visible-package"}"#.to_string()),
            directly_requested: Set(true),
            current_version: Set(Some("1.0.0".to_string())),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        packages::ActiveModel {
            name: Set("hidden-dependency".to_string()),
            pkgbase: Set("hidden-dependency".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"hidden-dependency"}"#.to_string()),
            directly_requested: Set(false),
            current_version: Set(Some("1.0.0".to_string())),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let packages = list_directly_requested_packages(&db, None, None)
            .await
            .unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "visible-package");
    }

    #[tokio::test]
    async fn package_dependencies_include_link_targets() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            pkgbase: Set("parent".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
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
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("2.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"child"}"#.to_string()),
            directly_requested: Set(false),
            current_version: Set(Some("2.0.0".to_string())),
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
            platforms: Set("x86_64".to_string()),
            version_constraint: Set(">=2.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let deps = list_package_relations(
            &db,
            parent.id,
            dependencies::Column::DependentId,
            dependencies::Column::DependeeId,
        )
        .await
        .unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, child.id);
        assert_eq!(deps[0].name, "child");
        assert_eq!(deps[0].version_constraint, ">=2.0");
    }

    #[tokio::test]
    async fn package_dependents_include_reverse_link_targets() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let dependency = packages::ActiveModel {
            name: Set("dependency".to_string()),
            pkgbase: Set("dependency".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"dependency"}"#.to_string()),
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

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            pkgbase: Set("parent".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("2.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(r#"{"type":"aur","name":"parent"}"#.to_string()),
            directly_requested: Set(true),
            current_version: Set(Some("2.0.0".to_string())),
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
            dependee_id: Set(dependency.id),
            platforms: Set("x86_64".to_string()),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let dependents = list_package_relations(
            &db,
            dependency.id,
            dependencies::Column::DependeeId,
            dependencies::Column::DependentId,
        )
        .await
        .unwrap();

        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].id, parent.id);
        assert_eq!(dependents[0].name, "parent");
        assert_eq!(dependents[0].version_constraint, ">=1.0");
    }
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get package details
This requires 1 API call to the AUR (rate limited 4000 per day)
https://wiki.archlinux.org/title/Aurweb_RPC_interface", body = ExtendedPackageModel),
    ),
    params(
            ("id", description = "Id of package")
    )
)]
#[get("/package/<id>")]
pub async fn get_package(
    db: &State<DatabaseConnection>,
    id: i32,
    _a: Authenticated,
) -> Result<Json<ExtendedPackageModel>, Custom<String>> {
    let db = db as &DatabaseConnection;

    let pkg = Packages::find()
        .filter(packages::Column::Id.eq(id))
        .one(db)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?
        .ok_or(Custom(Status::NotFound, "ID not found".to_string()))?;

    // Query the latest build.version for this package (most recent by end_time then start_time)
    let latest_version_row = Builds::find()
        .select_only()
        .column(builds::Column::Version)
        .filter(builds::Column::PkgId.eq(pkg.id))
        .order_by(builds::Column::EndTime, Order::Desc)
        .order_by(builds::Column::StartTime, Order::Desc)
        .limit(1)
        .into_tuple::<(String,)>()
        .one(db)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;

    let latest_version: Option<String> = latest_version_row.map(|(v,)| v);
    let dependencies = list_package_relations(
        db,
        pkg.id,
        dependencies::Column::DependentId,
        dependencies::Column::DependeeId,
    )
    .await
    .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;
    let dependents = list_package_relations(
        db,
        pkg.id,
        dependencies::Column::DependeeId,
        dependencies::Column::DependentId,
    )
    .await
    .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;

    let source_data = SourceData::from_str(pkg.source_data.as_str())
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;

    let (package_source, version) = match source_data {
        SourceData::Aur { .. } => {
            let aur_info = get_package_info(&pkg.pkgbase)
                .await
                .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;

            match aur_info {
                None => (
                    PackageSource::AurNotFound(AurNotFoundPackage {}),
                    pkg.upstream_version.unwrap_or_default(),
                ),
                Some(aur_info) => {
                    let aur_url = format!(
                        "https://aur.archlinux.org/packages/{}",
                        aur_info.package_base
                    );

                    (
                        PackageSource::Aur(AurPackage {
                            name: pkg.pkgbase.clone(),
                            project_url: aur_info.url,
                            description: aur_info.description,
                            last_updated: aur_info.last_modified,
                            first_submitted: aur_info.first_submitted,
                            licenses: aur_info.license.map(|l| l.join(", ")),
                            maintainer: aur_info.maintainer,
                            aur_flagged_outdated: aur_info.out_of_date.unwrap_or(0) != 0,
                            aur_url,
                        }),
                        aur_info.version,
                    )
                }
            }
        }
        SourceData::Git {
            subfolder,
            url,
            r#ref,
        } => (
            PackageSource::Git(GitPackage {
                git_url: url,
                git_ref: r#ref.clone(),
                subfolder,
            }),
            // This versions actuality dpendes on the update-version-check interval
            pkg.upstream_version.unwrap_or(String::new()),
        ),
        SourceData::Upload { .. } => {
            todo!("upload zip is not yet implemented")
        }
    };

    let ext_pkg = ExtendedPackageModel {
        id: pkg.id,
        name: pkg.pkgbase,
        directly_requested: pkg.directly_requested,
        status: pkg.status,
        outofdate: pkg.out_of_date,
        latest_version,
        package_source,
        selected_platforms: pkg.platforms.split(';').map(ToString::to_string).collect(),
        selected_build_flags: Some(
            pkg.build_flags
                .split(';')
                .map(ToString::to_string)
                .collect(),
        ),
        upstream_version: version,
        split_packages: pkg
            .split_packages
            .and_then(|s| serde_json::from_str(&s).ok()),
        dependencies,
        dependents,
    };

    Ok(Json(ext_pkg))
}
