use crate::models::authenticated::Authenticated;
use crate::models::input::{ExtendedPackageModel, SimplePackageModel};
use crate::models::package::{AddPackage, PackagePatchModel, UpdatePackage};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::package_add_activity::PackageAddActivity;
use aurcache_activitylog::package_delete_activity::PackageDeleteActivity;
use aurcache_activitylog::package_update_activity::PackageUpdateActivity;
use aurcache_builder::types::Action;
use aurcache_db::activities::ActivityType;
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, packages};
use aurcache_utils::aur::api::get_package_info;
use aurcache_utils::package::add::package_add;
use aurcache_utils::package::delete::package_delete;
use aurcache_utils::package::update::package_update;
use pacman_mirrors::platforms::Platform;
use rocket::http::Status;
use rocket::response::status::{BadRequest, Custom, NotFound};
use rocket::serde::json::Json;
use rocket::{State, delete, get, patch, post};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Expr;
use sea_orm::{ActiveModelTrait, DatabaseConnection, NotSet, Order};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use std::str::FromStr;
use tokio::sync::broadcast::Sender;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(
    package_add_endpoint,
    package_update_entity_endpoint,
    package_update_endpoint,
    package_del,
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

    package_add(
        db,
        input.name.clone(),
        tx,
        platforms,
        input.build_flags.clone(),
        input.source.clone(),
    )
    .await
    .map_err(|e| BadRequest(e.to_string()))?;

    al.add(
        PackageAddActivity {
            package: input.name.clone(),
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

    // Start building the update operation
    let update_pkg = packages::ActiveModel {
        id: Set(id),
        name: input.name.clone().map(Set).unwrap_or(NotSet),
        status: input.status.map(Set).unwrap_or(NotSet),
        out_of_date: input.out_of_date.map(Set).unwrap_or(NotSet),
        latest_aur_version: input.latest_aur_version.clone().map(Set).unwrap_or(NotSet),
        latest_build: input.latest_build.map(Set).unwrap_or(NotSet),
        build_flags: input
            .build_flags
            .clone()
            .map(|v| Set(v.join(";")))
            .unwrap_or(NotSet),
        platforms: input
            .platforms
            .clone()
            .map(|v| Set(v.join(";")))
            .unwrap_or(NotSet),
        source_type: NotSet,
        source_data: NotSet,
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
        .column_as(packages::Column::LatestAurVersion, "latest_aur_version")
        // wrap the correlated subquery in COALESCE -> fallback to empty string
        .column_as(
            Expr::cust(format!("COALESCE({}, '')", latest_version_subquery)),
            "latest_version",
        )
        .order_by(packages::Column::OutOfDate, Order::Desc)
        .order_by(packages::Column::Id, Order::Desc)
        .limit(limit)
        .offset(page.zip(limit).map(|(page, limit)| page * limit))
        .into_model::<SimplePackageModel>()
        .all(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?;

    Ok(Json(all))
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

    let aur_info = get_package_info(&pkg.name)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?
        .ok_or(Custom(Status::NotFound, "Package not found".to_string()))?;

    let aur_url = format!(
        "https://aur.archlinux.org/packages/{}",
        aur_info.package_base
    );

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

    let ext_pkg = ExtendedPackageModel {
        id: pkg.id,
        name: pkg.name,
        status: pkg.status,
        outofdate: pkg.out_of_date,
        latest_version,
        latest_aur_version: aur_info.version,
        last_updated: aur_info.last_modified,
        first_submitted: aur_info.first_submitted,
        licenses: aur_info.license.map(|l| l.join(", ")),
        maintainer: aur_info.maintainer,
        aur_flagged_outdated: aur_info.out_of_date.unwrap_or(0) != 0,
        selected_platforms: pkg.platforms.split(";").map(|v| v.to_string()).collect(),
        selected_build_flags: Some(pkg.build_flags.split(";").map(|v| v.to_string()).collect()),
        aur_url,
        project_url: aur_info.url,
        description: aur_info.description,
    };
    Ok(Json(ext_pkg))
}
