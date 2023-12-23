use crate::aur::aur::get_info_by_name;
use crate::builder::types::Action;
use crate::db::{packages, versions};
use crate::query_aur;
use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::State;
use rocket::{get, post, Route};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, openapi_get_routes, JsonSchema};
use sea_orm::EntityTrait;
use sea_orm::{ActiveModelTrait, DatabaseConnection, FromQueryResult, JoinType, QuerySelect, Set};
use sea_orm::{ColumnTrait, RelationTrait};
use tokio::sync::broadcast::Sender;

use crate::db::prelude::Packages;
use crate::repo::repo::{remove_pkg, remove_version};

#[derive(Serialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
struct ApiPackage {
    name: String,
    version: String,
}

#[openapi(tag = "test")]
#[get("/search?<query>")]
async fn search(query: &str) -> Result<Json<Vec<ApiPackage>>, String> {
    match query_aur(query).await {
        Ok(v) => {
            let mapped = v
                .iter()
                .map(|x| ApiPackage {
                    name: x.name.clone(),
                    version: x.version.clone(),
                })
                .collect();
            return Ok(Json(mapped));
        }
        Err(e) => {
            return Err(format!("{}", e));
        }
    }
}

#[derive(FromQueryResult, Deserialize, JsonSchema, Serialize)]
#[serde(crate = "rocket::serde")]
struct ListPackageModel {
    name: String,
    count: i32,
}

#[openapi(tag = "test")]
#[get("/packages/list")]
async fn package_list(
    db: &State<DatabaseConnection>,
) -> Result<Json<Vec<ListPackageModel>>, String> {
    let db = db as &DatabaseConnection;

    let all: Vec<ListPackageModel> = Packages::find()
        .join_rev(JoinType::InnerJoin, versions::Relation::Packages.def())
        .select_only()
        .column_as(versions::Column::Id.count(), "count")
        .column(packages::Column::Name)
        .group_by(packages::Column::Name)
        .into_model::<ListPackageModel>()
        .all(db)
        .await
        .unwrap();

    Ok(Json(all))
}

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
struct AddBody {
    name: String,
}

#[openapi(tag = "test")]
#[post("/packages/add", data = "<input>")]
async fn package_add(
    db: &State<DatabaseConnection>,
    input: Json<AddBody>,
    tx: &State<Sender<Action>>,
) -> Result<(), String> {
    let db = db as &DatabaseConnection;
    let pkg_name = &input.name;

    let pkg = get_info_by_name(pkg_name)
        .await
        .map_err(|_| "couldn't download package metadata".to_string())?;

    let new_package = packages::ActiveModel {
        name: Set(pkg_name.clone()),
        ..Default::default()
    };

    let pkt_model = new_package.save(db).await.expect("TODO: panic message");

    let new_version = versions::ActiveModel {
        version: Set(pkg.version.clone()),
        package_id: Set(pkt_model.id.clone().unwrap()),
        ..Default::default()
    };

    let version_model = new_version.save(db).await.expect("TODO: panic message");

    let _ = tx.send(Action::Build(
        pkg.name,
        pkg.version,
        pkg.url_path.unwrap(),
        version_model,
    ));

    Ok(())
}

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
struct DelBody {
    id: i32,
}

#[openapi(tag = "test")]
#[post("/packages/delete", data = "<input>")]
async fn package_del(db: &State<DatabaseConnection>, input: Json<DelBody>) -> Result<(), String> {
    let db = db as &DatabaseConnection;
    let pkg_id = input.id.clone();

    remove_pkg(db, pkg_id).await.map_err(|e| e.to_string())?;

    Ok(())
}

#[openapi(tag = "test")]
#[post("/versions/delete/<id>")]
async fn version_del(db: &State<DatabaseConnection>, id: i32) -> Result<(), String> {
    let db = db as &DatabaseConnection;

    remove_version(db, id).await.map_err(|e| e.to_string())?;

    Ok(())
}

pub fn build_api() -> Vec<Route> {
    openapi_get_routes![search, package_list, package_add, package_del, version_del]
}
