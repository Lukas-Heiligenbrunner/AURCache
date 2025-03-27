use crate::db::migration::{JoinType, Order};
use crate::db::prelude::Builds;
use crate::db::{builds, packages};
use itertools::Itertools;
use rocket::response::status::NotFound;
use rocket::serde::json::Json;
use rocket::{State, delete, get, post};

use crate::api::models::authenticated::Authenticated;
use crate::api::models::input::ListBuildsModel;
use crate::builder::types::{Action, BuildStates};
use crate::package::update::update_platform;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, QueryFilter,
    QueryOrder, QuerySelect, RelationTrait, Set,
};
use tokio::sync::broadcast::Sender;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(
    build_output,
    list_builds,
    get_build,
    delete_build,
    cancel_build,
    rery_build
))]
pub struct BuildApi;

#[utoipa::path(
    responses(
            (status = 200, description = "get build output of specified build"),
    ),
    params(
            ("buildid", description = "Id of build"),
            ("startline", description = "Startline to fetch from (only content from this line on)")
    )
)]
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

    match build.output {
        None => Err(NotFound("No Output".to_string())),
        Some(v) => match startline {
            None => Ok(v),
            Some(startline) => {
                let output = v.lines().skip(startline as usize).join("\n");
                Ok(output)
            }
        },
    }
}

#[utoipa::path(
    responses(
            (status = 200, description = "List of all builds"),
    ),
    params(
            ("pkgid", description = "Id of Package"),
            ("limit", description = "Limit of items to fetch"),
            ("page", description = "Page to fetch")
    )
)]
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

#[utoipa::path(
    responses(
            (status = 200, description = "Get build details"),
    ),
    params(
            ("buildid", description = "Id of build")
    )
)]
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

#[utoipa::path(
    responses(
            (status = 200, description = "Delete build"),
    ),
    params(
            ("buildid", description = "Id of build")
    )
)]
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

#[utoipa::path(
    responses(
            (status = 200, description = "Cancel build job"),
    ),
    params(
            ("buildid", description = "Id of build")
    )
)]
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

#[utoipa::path(
    responses(
            (status = 200, description = "Retry build"),
    ),
    params(
            ("buildid", description = "Id of build"),
    )
)]
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
