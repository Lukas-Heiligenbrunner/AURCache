use crate::builder::types::Action;
use crate::db::migration::Order;
use crate::db::packages;
use crate::db::prelude::Packages;
use crate::package::add::package_add;
use crate::package::delete::package_delete;
use crate::package::update::package_update;
use rocket::response::status::{BadRequest, NotFound};
use rocket::serde::json::Json;

use rocket::{delete, get, post, State};

use crate::api::types::authenticated::Authenticated;
use crate::api::types::input::{ExtendedPackageModel, SimplePackageModel};
use crate::api::types::output::{AddBody, UpdateBody};
use crate::aur::api::get_info_by_name;
use rocket_okapi::openapi;
use sea_orm::DatabaseConnection;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use tokio::sync::broadcast::Sender;

/// Add new Package to build queue
#[openapi(tag = "Packages")]
#[post("/package", data = "<input>")]
pub async fn package_add_endpoint(
    db: &State<DatabaseConnection>,
    input: Json<AddBody>,
    tx: &State<Sender<Action>>,
    _a: Authenticated,
) -> Result<(), BadRequest<String>> {
    package_add(db, input.name.clone(), tx)
        .await
        .map_err(|e| BadRequest(e.to_string()))
}

/// Update a package with id
#[openapi(tag = "Packages")]
#[post("/package/<id>/update", data = "<input>")]
pub async fn package_update_endpoint(
    db: &State<DatabaseConnection>,
    id: i32,
    input: Json<UpdateBody>,
    tx: &State<Sender<Action>>,
    _a: Authenticated,
) -> Result<Json<i32>, BadRequest<String>> {
    package_update(db, id, input.force, tx)
        .await
        .map(Json)
        .map_err(|e| BadRequest(e.to_string()))
}

/// Delete package with id
#[openapi(tag = "Packages")]
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
#[openapi(tag = "Packages")]
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
#[openapi(tag = "Packages")]
#[get("/package/<id>")]
pub async fn get_package(
    db: &State<DatabaseConnection>,
    id: u64,
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

    let ext_pkg = ExtendedPackageModel {
        id: pkg.id,
        name: pkg.name,
        status: pkg.status,
        outofdate: pkg.out_of_date,
        latest_version: Some(pkg.version), // todo might be null in databse right?
        latest_aur_version: aur_info.version,
        last_updated: aur_info.last_modified,
        first_submitted: aur_info.first_submitted,
        licenses: aur_info.license.map(|l| l.join(", ")),
        maintainer: aur_info.maintainer,
        aur_flagged_outdated: aur_info.out_of_date.unwrap_or(0) != 0,
        selected_platforms: vec![], // todo add those two to db
        selected_build_flags: vec![],
        aur_url: format!(
            "https://aur.archlinux.org/packages/{}",
            aur_info.package_base
        ),
        project_url: aur_info.url,
        description: aur_info.description,
    };
    Ok(Json(ext_pkg))
}
