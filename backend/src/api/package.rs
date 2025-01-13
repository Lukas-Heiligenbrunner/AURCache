use crate::builder::types::Action;
use crate::db::migration::Order;
use crate::db::packages;
use crate::db::prelude::Packages;
use crate::package::add::package_add;
use crate::package::delete::package_delete;
use crate::package::update::package_update;
use rocket::response::status::{BadRequest, NotFound};
use rocket::serde::json::Json;

use rocket::{delete, get, patch, post, State};

use crate::api::types::authenticated::Authenticated;
use crate::api::types::input::{ExtendedPackageModel, PackagePatchModel, SimplePackageModel};
use crate::api::types::output::{AddBody, UpdateBody};
use crate::aur::api::get_info_by_name;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, DatabaseConnection, NotSet};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
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
    input: Json<AddBody>,
    tx: &State<Sender<Action>>,
    _a: Authenticated,
) -> Result<(), BadRequest<String>> {
    package_add(
        db,
        input.name.clone(),
        tx,
        input.platforms.clone(),
        input.build_flags.clone(),
    )
    .await
    .map_err(|e| BadRequest(e.to_string()))
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
        version: input.version.clone().map(Set).unwrap_or(NotSet),
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
    };

    // Execute the update query
    update_pkg
        .update(db)
        .await
        .map_err(|e| BadRequest(e.to_string()))?;

    Ok(())
}

/// Update a package with id
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
    input: Json<UpdateBody>,
    tx: &State<Sender<Action>>,
    _a: Authenticated,
) -> Result<Json<Vec<i32>>, BadRequest<String>> {
    package_update(db, id, input.force, tx)
        .await
        .map(Json)
        .map_err(|e| BadRequest(e.to_string()))
}

/// Delete package with id
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
    _a: Authenticated,
) -> Result<(), String> {
    package_delete(db, id).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Get all packages currently in Repo
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

    let all: Vec<SimplePackageModel> = Packages::find()
        .select_only()
        .column(packages::Column::Name)
        .column(packages::Column::Id)
        .column(packages::Column::Status)
        .column_as(packages::Column::OutOfDate, "outofdate")
        .column_as(packages::Column::LatestAurVersion, "latest_aur_version")
        .column_as(packages::Column::Version, "latest_version")
        .order_by(packages::Column::Id, Order::Desc)
        .limit(limit)
        .offset(page.zip(limit).map(|(page, limit)| page * limit))
        .into_model::<SimplePackageModel>()
        .all(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?;

    Ok(Json(all))
}

/// get specific package by id
/// This requires 1 API call to the AUR (rate limited 4000 per day)
/// https://wiki.archlinux.org/title/Aurweb_RPC_interface
#[utoipa::path(
    responses(
            (status = 200, description = "Get package details", body = ExtendedPackageModel),
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
) -> Result<Json<ExtendedPackageModel>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let pkg = Packages::find()
        .filter(packages::Column::Id.eq(id))
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
        .ok_or(NotFound("id not found".to_string()))?;

    let aur_info = get_info_by_name(&pkg.name)
        .await
        .map_err(|e| NotFound(e.to_string()))?;

    let aur_url = format!(
        "https://aur.archlinux.org/packages/{}",
        aur_info.package_base
    );

    let ext_pkg = ExtendedPackageModel {
        id: pkg.id,
        name: pkg.name,
        status: pkg.status,
        outofdate: pkg.out_of_date,
        latest_version: pkg.version,
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
