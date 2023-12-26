use crate::aur::aur::query_aur;
use crate::db::migration::JoinType;
use crate::db::prelude::{Builds, Packages};
use crate::db::{builds, packages, versions};
use rocket::response::status::NotFound;
use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::{get, State};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, JsonSchema};
use sea_orm::{ColumnTrait, QueryFilter};
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
    return match query_aur(query).await {
        Ok(v) => {
            let mapped = v
                .iter()
                .map(|x| ApiPackage {
                    name: x.name.clone(),
                    version: x.version.clone(),
                })
                .collect();
            Ok(Json(mapped))
        }
        Err(e) => Err(format!("{}", e)),
    };
}

#[derive(FromQueryResult, Deserialize, JsonSchema, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct ListPackageModel {
    id: i32,
    name: String,
    count: i32,
    status: i32,
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
        .column(packages::Column::Id)
        .column(packages::Column::Status)
        .group_by(packages::Column::Name)
        .into_model::<ListPackageModel>()
        .all(db)
        .await
        .unwrap();

    Ok(Json(all))
}

#[openapi(tag = "test")]
#[get("/builds/output?<buildid>")]
pub async fn build_output(
    db: &State<DatabaseConnection>,
    buildid: i32,
) -> Result<String, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let build = Builds::find_by_id(buildid)
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
        .ok_or(NotFound("couldn't find id".to_string()))?;

    build.ouput.ok_or(NotFound("No Output".to_string()))
}

#[derive(FromQueryResult, Deserialize, JsonSchema, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct ListBuildsModel {
    id: i32,
    pkg_id: i32,
    version_id: i32,
    status: Option<i32>,
}

#[openapi(tag = "test")]
#[get("/builds?<pkgid>")]
pub async fn list_builds(
    db: &State<DatabaseConnection>,
    pkgid: i32,
) -> Result<Json<Vec<ListBuildsModel>>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let build = Builds::find()
        .filter(builds::Column::PkgId.eq(pkgid))
        .all(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?;

    Ok(Json(
        build
            .iter()
            .map(|x| ListBuildsModel {
                id: x.id,
                status: x.status,
                pkg_id: x.pkg_id,
                version_id: x.version_id,
            })
            .collect::<Vec<_>>(),
    ))
}
