use crate::aur::aur::query_aur;
use crate::db::migration::JoinType;
use crate::db::prelude::Packages;
use crate::db::{packages, versions};
use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::{get, State};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, JsonSchema};
use sea_orm::ColumnTrait;
use sea_orm::{DatabaseConnection, EntityTrait, FromQueryResult, QuerySelect, RelationTrait};

#[derive(Serialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct ApiPackage {
    name: String,
    version: String,
}

#[openapi(tag = "test")]
#[get("/search?<query>")]
pub async fn search(query: &str) -> Result<Json<Vec<ApiPackage>>, String> {
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
pub struct ListPackageModel {
    name: String,
    count: i32,
}

#[openapi(tag = "test")]
#[get("/packages/list")]
pub async fn package_list(
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
