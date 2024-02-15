use crate::api::list::ListPackageModel;
use crate::aur::aur::get_info_by_name;
use crate::builder::types::Action;
use crate::db::migration::{JoinType, Order};
use crate::db::prelude::{Packages, Versions};
use crate::db::{packages, versions};
use crate::repo::repo::remove_pkg;
use rocket::response::status::{BadRequest, NotFound};
use rocket::serde::json::Json;
use rocket::serde::Deserialize;
use rocket::{get, post, State};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, JsonSchema};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    RelationTrait, TransactionTrait,
};
use sea_orm::{DatabaseConnection, Set};
use tokio::sync::broadcast::Sender;

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct AddBody {
    name: String,
}

#[openapi(tag = "Packages")]
#[post("/packages/add", data = "<input>")]
pub async fn package_add(
    db: &State<DatabaseConnection>,
    input: Json<AddBody>,
    tx: &State<Sender<Action>>,
) -> Result<(), BadRequest<String>> {
    let txn = db
        .begin()
        .await
        .map_err(|e| BadRequest(Some(e.to_string())))?;

    // remove leading and trailing whitespaces
    let pkg_name = input.name.trim();

    if let Some(..) = Packages::find()
        .filter(packages::Column::Name.eq(pkg_name))
        .one(&txn)
        .await
        .map_err(|e| BadRequest(Some(e.to_string())))?
    {
        return Err(BadRequest(Some("Package already exists".to_string())));
    }

    let pkg = get_info_by_name(pkg_name)
        .await
        .map_err(|_| BadRequest(Some("couldn't download package metadata".to_string())))?;

    let mut new_package = packages::ActiveModel {
        name: Set(pkg_name.to_string()),
        status: Set(3),
        latest_aur_version: Set(pkg.version.clone()),
        ..Default::default()
    };

    let mut new_package = new_package
        .clone()
        .save(&txn)
        .await
        .map_err(|e| BadRequest(Some(e.to_string())))?;

    let new_version = versions::ActiveModel {
        version: Set(pkg.version.clone()),
        package_id: new_package.id.clone(),
        ..Default::default()
    };

    let new_version = new_version
        .clone()
        .save(&txn)
        .await
        .map_err(|e| BadRequest(Some(e.to_string())))?;

    new_package.status = Set(3);
    new_package.latest_version_id = Set(Some(new_version.id.clone().unwrap()));
    new_package
        .save(&txn)
        .await
        .map_err(|e| BadRequest(Some(e.to_string())))?;

    let _ = tx.send(Action::Build(
        pkg.name,
        pkg.version,
        pkg.url_path.unwrap(),
        new_version,
    ));

    txn.commit()
        .await
        .map_err(|e| BadRequest(Some(e.to_string())))?;
    Ok(())
}

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct UpdateBody {
    force: bool,
}

#[openapi(tag = "Packages")]
#[post("/packages/<id>/update", data = "<input>")]
pub async fn package_update(
    db: &State<DatabaseConnection>,
    id: i32,
    input: Json<UpdateBody>,
    tx: &State<Sender<Action>>,
) -> Result<(), BadRequest<String>> {
    let db = db as &DatabaseConnection;

    let mut pkg_model: packages::ActiveModel = Packages::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| BadRequest(Some(e.to_string())))?
        .ok_or(BadRequest(Some("id not found".to_string())))?
        .into();

    let pkg = get_info_by_name(pkg_model.name.clone().unwrap().as_str())
        .await
        .map_err(|_| BadRequest(Some("couldn't download package metadata".to_string())))?;

    let version_model = match Versions::find()
        .filter(versions::Column::Version.eq(pkg.version.clone()))
        .filter(versions::Column::PackageId.eq(pkg.id.clone()))
        .one(db)
        .await
        .map_err(|e| BadRequest(Some(e.to_string())))?
    {
        None => {
            let new_version = versions::ActiveModel {
                version: Set(pkg.version.clone()),
                package_id: Set(pkg_model.id.clone().unwrap()),
                ..Default::default()
            };

            new_version.save(db).await.expect("TODO: panic message")
        }
        Some(p) => {
            // todo add check if this version was successfully built
            // if not allow build
            if input.force {
                p.into()
            } else {
                return Err(BadRequest(Some("Version already existing".to_string())));
            }
        }
    };

    pkg_model.status = Set(3);
    pkg_model.latest_version_id = Set(Some(version_model.id.clone().unwrap()));
    pkg_model.save(db).await.expect("todo error message");

    let _ = tx.send(Action::Build(
        pkg.name,
        pkg.version,
        pkg.url_path.unwrap(),
        version_model,
    ));

    Ok(())
}

#[openapi(tag = "Packages")]
#[post("/package/delete/<id>")]
pub async fn package_del(db: &State<DatabaseConnection>, id: i32) -> Result<(), String> {
    let db = db as &DatabaseConnection;

    remove_pkg(db, id).await.map_err(|e| e.to_string())?;

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
