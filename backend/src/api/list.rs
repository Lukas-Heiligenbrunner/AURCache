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
#[get("/builds/output?<buildid>&<startline>")]
pub async fn build_output(
    db: &State<DatabaseConnection>,
    buildid: i32,
    startline: Option<i32>,
) -> Result<String, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let build = Builds::find_by_id(buildid)
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
        .ok_or(NotFound("couldn't find id".to_string()))?;

    return match build.ouput {
        None => Err(NotFound("No Output".to_string())),
        Some(v) => match startline {
            None => Ok(v),
            Some(startline) => {
                let output: Vec<String> = v.split("\n").map(|x| x.to_string()).collect();
                let len = output.len();
                let len_missing = len as i32 - startline;

                let output = output
                    .iter()
                    .rev()
                    .take(if len_missing > 0 {
                        len_missing as usize
                    } else {
                        0
                    })
                    .rev()
                    .map(|x1| x1.clone())
                    .collect::<Vec<_>>();

                let output = output.join("\n");
                Ok(output)
            }
        },
    };
}

#[derive(FromQueryResult, Deserialize, JsonSchema, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct ListBuildsModel {
    id: i32,
    pkg_name: String,
    version: String,
    status: i32,
}

#[openapi(tag = "test")]
#[get("/builds?<pkgid>")]
pub async fn list_builds(
    db: &State<DatabaseConnection>,
    pkgid: Option<i32>,
) -> Result<Json<Vec<ListBuildsModel>>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let basequery = Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .join_rev(JoinType::InnerJoin, versions::Relation::Builds.def())
        .select_only()
        .column_as(builds::Column::Id, "id")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column(versions::Column::Version);

    let build = match pkgid {
        None => basequery.into_model::<ListBuildsModel>().all(db),
        Some(pkgid) => basequery
            .filter(builds::Column::PkgId.eq(pkgid))
            .into_model::<ListBuildsModel>()
            .all(db),
    }
    .await
    .map_err(|e| NotFound(e.to_string()))?;

    Ok(Json(build))
}
