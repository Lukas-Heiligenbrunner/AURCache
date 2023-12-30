use crate::aur::aur::query_aur;
use crate::db::migration::JoinType;
use crate::db::prelude::{Builds, Packages};
use crate::db::{builds, packages, versions};
use crate::utils::dir_size::dir_size;
use rocket::response::status::NotFound;
use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::{get, State};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, JsonSchema};
use sea_orm::{PaginatorTrait};
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
        .column(versions::Column::Version)
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

#[openapi(tag = "test")]
#[get("/builds/<buildid>")]
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
        .column(versions::Column::Version)
        .into_model::<ListBuildsModel>()
        .one(db).await.map_err(|e| NotFound(e.to_string()))?.ok_or(NotFound("no item with id found".to_string()))?;

    Ok(Json(result))
}

#[derive(FromQueryResult, Deserialize, JsonSchema, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct ListStats {
    total_builds: u32,
    failed_builds: u32,
    avg_queue_wait_time: u32,
    avg_build_time: u32,
    repo_storage_size: u32,
    active_builds: u32,
    total_packages: u32,
}

#[openapi(tag = "test")]
#[get("/stats")]
pub async fn stats(db: &State<DatabaseConnection>) -> Result<Json<ListStats>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    return match get_stats(db).await {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(NotFound(e.to_string())),
    };
}

async fn get_stats(db: &DatabaseConnection) -> anyhow::Result<ListStats> {
    // Count total builds
    let total_builds: u32 = Builds::find().count(db).await?.try_into()?;

    // Count failed builds
    let failed_builds: u32 = Builds::find()
        .filter(builds::Column::Status.eq(2))
        .count(db)
        .await?
        .try_into()?;

    // todo implement this values somehow
    let avg_queue_wait_time: u32 = 42;
    let avg_build_time: u32 = 42;

    // Calculate repo storage size
    let repo_storage_size: u32 = dir_size("repo/").unwrap_or(0).try_into()?;

    // Count active builds
    let active_builds: u32 = Builds::find()
        .filter(builds::Column::Status.eq(0))
        .count(db)
        .await?
        .try_into()?;

    // Count total packages
    let total_packages: u32 = Packages::find().count(db).await?.try_into()?;

    Ok(ListStats {
        total_builds,
        failed_builds,
        avg_queue_wait_time,
        avg_build_time,
        repo_storage_size,
        active_builds,
        total_packages,
    })
}
