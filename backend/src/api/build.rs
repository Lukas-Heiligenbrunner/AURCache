use crate::db::migration::{JoinType, Order};
use crate::db::prelude::Builds;
use crate::db::{builds, packages};
use rocket::response::status::NotFound;
use rocket::serde::json::Json;
use rocket::{delete, get, post, State};

use crate::api::types::authenticated::Authenticated;
use crate::api::types::input::ListBuildsModel;
use crate::builder::types::{Action, BuildStates};
use crate::package::update::update_platform;
use rocket_okapi::openapi;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, QueryFilter,
    QueryOrder, QuerySelect, RelationTrait, Set,
};
use tokio::sync::broadcast::Sender;

/// Get build output of specified build
/// use startline to specify a start-line (to fetch only new content)
#[openapi(tag = "build")]
#[get("/build/<buildid>/output?<startline>")]
pub async fn build_output(
    db: &State<DatabaseConnection>,
    buildid: i32,
    startline: Option<i32>,
    _a: Authenticated,
) -> Result<String, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let build = Builds::find_by_id(buildid)
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
        .ok_or(NotFound("couldn't find id".to_string()))?;

    return match build.output {
        None => Err(NotFound("No Output".to_string())),
        Some(v) => match startline {
            None => Ok(v),
            Some(startline) => {
                let output: Vec<String> = v.split('\n').map(|x| x.to_string()).collect();
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
                    .cloned()
                    .collect::<Vec<_>>();

                let output = output.join("\n");
                Ok(output)
            }
        },
    };
}

/// get list of all builds
#[openapi(tag = "build")]
#[get("/builds?<pkgid>&<limit>&<page>")]
pub async fn list_builds(
    db: &State<DatabaseConnection>,
    pkgid: Option<i32>,
    limit: Option<u64>,
    page: Option<u64>,
    _a: Authenticated,
) -> Result<Json<Vec<ListBuildsModel>>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let basequery = Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .select_only()
        .column_as(builds::Column::Id, "id")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column_as(packages::Column::Id, "pkg_id")
        .column(packages::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .column(builds::Column::Platform)
        .order_by(builds::Column::StartTime, Order::Desc)
        .limit(limit)
        .offset(page.zip(limit).map(|(page, limit)| page * limit));

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
    _a: Authenticated,
) -> Result<Json<ListBuildsModel>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let result = Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .filter(builds::Column::Id.eq(buildid))
        .select_only()
        .column_as(builds::Column::Id, "id")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column_as(packages::Column::Id, "pkg_id")
        .column(packages::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .column(builds::Column::Platform)
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
    _a: Authenticated,
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

#[openapi(tag = "build")]
#[post("/build/<buildid>/cancel")]
pub async fn cancel_build(
    tx: &State<Sender<Action>>,
    buildid: i32,
    _a: Authenticated,
) -> Result<(), NotFound<String>> {
    let _ = tx
        .send(Action::Cancel(buildid))
        .map_err(|e| NotFound(e.to_string()))?;

    Ok(())
}

#[openapi(tag = "build")]
#[post("/build/<buildid>/retry")]
pub async fn rery_build(
    db: &State<DatabaseConnection>,
    tx: &State<Sender<Action>>,
    buildid: i32,
    _a: Authenticated,
) -> Result<Json<i32>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    // Fetch the build details
    let old_build = Builds::find_by_id(buildid)
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
        .ok_or(NotFound("Build not found".to_string()))?;

    // Extract the platform and package ID
    let platform = old_build.platform;
    let pkg_id = old_build.pkg_id;

    // Fetch the package details
    let package = packages::Entity::find_by_id(pkg_id)
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
        .ok_or(NotFound("Package not found".to_string()))?;

    let mut pacage_am: packages::ActiveModel = package.clone().into();
    pacage_am.status = Set(BuildStates::ENQUEUED_BUILD);
    pacage_am
        .save(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?;

    let new_buildid = update_platform(&platform, package, db, tx)
        .await
        .map_err(|e| NotFound(e.to_string()))?;

    Ok(Json(new_buildid))
}
