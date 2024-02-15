use crate::api::list::ListPackageModel;
use crate::builder::types::Action;
use crate::db::migration::{JoinType, Order};
use crate::db::prelude::Packages;
use crate::db::{packages, versions};
use crate::package::add::package_add;
use crate::package::delete::package_delete;
use crate::package::update::package_update;
use rocket::response::status::{BadRequest, NotFound};
use rocket::serde::json::Json;
use rocket::serde::Deserialize;
use rocket::{get, post, State};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, JsonSchema};
use sea_orm::DatabaseConnection;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, RelationTrait};
use tokio::sync::broadcast::Sender;

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct AddBody {
    name: String,
}

#[openapi(tag = "Packages")]
#[post("/packages/add", data = "<input>")]
pub async fn package_add_endpoint(
    db: &State<DatabaseConnection>,
    input: Json<AddBody>,
    tx: &State<Sender<Action>>,
) -> Result<(), BadRequest<String>> {
    package_add(db, input.name.clone(), tx)
        .await
        .map_err(|e| BadRequest(Some(e.to_string())))
}

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct UpdateBody {
    force: bool,
}

#[openapi(tag = "Packages")]
#[post("/packages/<id>/update", data = "<input>")]
pub async fn package_update_endpoint(
    db: &State<DatabaseConnection>,
    id: i32,
    input: Json<UpdateBody>,
    tx: &State<Sender<Action>>,
) -> Result<(), BadRequest<String>> {
    package_update(db, id, input.force, tx)
        .await
        .map_err(|e| BadRequest(Some(e.to_string())))
}

#[openapi(tag = "Packages")]
#[post("/package/delete/<id>")]
pub async fn package_del(db: &State<DatabaseConnection>, id: i32) -> Result<(), String> {
    let db = db as &DatabaseConnection;

    package_delete(db, id).await.map_err(|e| e.to_string())?;

    Ok(())
}

#[openapi(tag = "Packages")]
#[get("/packages/list?<limit>")]
pub async fn package_list(
    db: &State<DatabaseConnection>,
    limit: Option<u64>,
) -> Result<Json<Vec<ListPackageModel>>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let all: Vec<ListPackageModel> = Packages::find()
        .join_rev(JoinType::LeftJoin, versions::Relation::LatestPackage.def())
        .select_only()
        .column(packages::Column::Name)
        .column(packages::Column::Id)
        .column(packages::Column::Status)
        .column_as(packages::Column::OutOfDate, "outofdate")
        .column_as(packages::Column::LatestAurVersion, "latest_aur_version")
        .column_as(versions::Column::Version, "latest_version")
        .column_as(packages::Column::LatestVersionId, "latest_version_id")
        .order_by(packages::Column::Id, Order::Desc)
        .limit(limit)
        .into_model::<ListPackageModel>()
        .all(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?;

    Ok(Json(all))
}

#[openapi(tag = "Packages")]
#[get("/package/<id>")]
pub async fn get_package(
    db: &State<DatabaseConnection>,
    id: u64,
) -> Result<Json<ListPackageModel>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let all: ListPackageModel = Packages::find()
        .join_rev(JoinType::LeftJoin, versions::Relation::LatestPackage.def())
        .filter(packages::Column::Id.eq(id))
        .select_only()
        .column(packages::Column::Name)
        .column(packages::Column::Id)
        .column(packages::Column::Status)
        .column_as(packages::Column::OutOfDate, "outofdate")
        .column_as(packages::Column::LatestAurVersion, "latest_aur_version")
        .column_as(versions::Column::Version, "latest_version")
        .column_as(packages::Column::LatestVersionId, "latest_version_id")
        .into_model::<ListPackageModel>()
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
        .ok_or(NotFound("id not found".to_string()))?;

    Ok(Json(all))
}
