use crate::db::migration::{JoinType, Order};
use crate::db::prelude::Builds;
use crate::db::{builds, packages, versions};
use rocket::response::status::NotFound;
use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::{delete, get, State};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, JsonSchema};
use sea_orm::ColumnTrait;
use sea_orm::QueryFilter;
use sea_orm::{
    DatabaseConnection, EntityTrait, FromQueryResult, ModelTrait, QueryOrder, QuerySelect,
    RelationTrait,
};

#[derive(FromQueryResult, Deserialize, JsonSchema, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct ListPackageModel {
    id: i32,
    name: String,
    status: i32,
    outofdate: bool,
    latest_version: Option<String>,
    latest_version_id: Option<i32>,
    latest_aur_version: String,
}

#[openapi(tag = "build")]
#[get("/build/<buildid>/output?<startline>")]
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
    pkg_id: i32,
    pkg_name: String,
    version: String,
    status: i32,
    start_time: Option<u32>,
    end_time: Option<u32>,
}

#[openapi(tag = "build")]
#[get("/builds?<pkgid>&<limit>")]
pub async fn list_builds(
    db: &State<DatabaseConnection>,
    pkgid: Option<i32>,
    limit: Option<u64>,
) -> Result<Json<Vec<ListBuildsModel>>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let basequery = Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .join_rev(JoinType::InnerJoin, versions::Relation::Builds.def())
        .select_only()
        .column_as(builds::Column::Id, "id")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column_as(packages::Column::Id, "pkg_id")
        .column(versions::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .order_by(builds::Column::StartTime, Order::Desc)
        .limit(limit);

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

#[openapi(tag = "build")]
#[get("/build/<buildid>")]
pub async fn get_build(
    db: &State<DatabaseConnection>,
    buildid: i32,
) -> Result<Json<ListBuildsModel>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let result = Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .join_rev(JoinType::InnerJoin, versions::Relation::Builds.def())
        .filter(builds::Column::Id.eq(buildid))
        .select_only()
        .column_as(builds::Column::Id, "id")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column_as(packages::Column::Id, "pkg_id")
        .column(versions::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .into_model::<ListBuildsModel>()
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
        .ok_or(NotFound("no item with id found".to_string()))?;

    Ok(Json(result))
}

#[openapi(tag = "build")]
#[delete("/build/<buildid>")]
pub async fn delete_build(
    db: &State<DatabaseConnection>,
    buildid: i32,
) -> Result<(), NotFound<String>> {
    let db = db as &DatabaseConnection;

    let build = Builds::find_by_id(buildid)
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
        .ok_or(NotFound("Id not found".to_string()))?;

    build
        .delete(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?;

    Ok(())
}