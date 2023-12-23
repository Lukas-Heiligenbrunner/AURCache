use crate::aur::aur::get_info_by_name;
use crate::builder::types::Action;
use crate::db::packages;
use crate::query_aur;
use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::State;
use rocket::{get, post, Route};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, openapi_get_routes, JsonSchema};
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use sea_orm::{DeleteResult, EntityTrait, ModelTrait};
use tokio::sync::broadcast::Sender;

use crate::db::prelude::Packages;
use crate::repo::repo::remove_pkg;

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

#[openapi(tag = "test")]
#[get("/packages/list")]
async fn package_list(
    db: &State<DatabaseConnection>,
) -> Result<Json<Vec<packages::Model>>, String> {
    let db = db as &DatabaseConnection;

    let all: Vec<packages::Model> = Packages::find().all(db).await.unwrap();

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
        version: Set(pkg.version.clone()),
        ..Default::default()
    };

    let t = new_package.save(db).await.expect("TODO: panic message");

    let _ = tx.send(Action::Build(
        pkg.name,
        pkg.version,
        pkg.url_path.unwrap(),
        t.id.unwrap(),
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
    let pkg_id = &input.id;

    let pkg = Packages::find_by_id(*pkg_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();

    // remove folders
    remove_pkg(pkg.name.to_string(), pkg.version.to_string()).await;

    // remove package db entry
    let res: DeleteResult = pkg.delete(db).await.unwrap();
    Ok(())
}

pub fn build_api() -> Vec<Route> {
    openapi_get_routes![search, package_list, package_add, package_del]
}
