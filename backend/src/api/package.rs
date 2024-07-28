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

use crate::api::types::input::ListPackageModel;
use crate::api::types::output::{AddBody, UpdateBody};
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
) -> Result<Json<i32>, BadRequest<String>> {
    package_update(db, id, input.force, tx)
        .await
        .map(Json)
        .map_err(|e| BadRequest(e.to_string()))
}

/// Delete package with id
#[openapi(tag = "Packages")]
#[delete("/package/<id>")]
pub async fn package_del(db: &State<DatabaseConnection>, id: i32) -> Result<(), String> {
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
) -> Result<Json<Vec<ListPackageModel>>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let all: Vec<ListPackageModel> = Packages::find()
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
        .into_model::<ListPackageModel>()
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
) -> Result<Json<ListPackageModel>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let all: ListPackageModel = Packages::find()
        .filter(packages::Column::Id.eq(id))
        .select_only()
        .column(packages::Column::Name)
        .column(packages::Column::Id)
        .column(packages::Column::Status)
        .column_as(packages::Column::OutOfDate, "outofdate")
        .column_as(packages::Column::LatestAurVersion, "latest_aur_version")
        .column_as(packages::Column::Version, "latest_version")
        .into_model::<ListPackageModel>()
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
        .ok_or(NotFound("id not found".to_string()))?;

    Ok(Json(all))
}
