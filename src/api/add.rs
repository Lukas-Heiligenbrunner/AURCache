use crate::aur::aur::get_info_by_name;
use crate::builder::types::Action;
use crate::db::prelude::{Packages, Versions};
use crate::db::{packages, versions};
use rocket::response::status::NotFound;
use rocket::serde::json::Json;
use rocket::serde::Deserialize;
use rocket::{post, State};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, JsonSchema};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};
use sea_orm::{DatabaseConnection, Set};
use tokio::sync::broadcast::Sender;

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct AddBody {
    name: String,
    force_build: bool,
}

#[openapi(tag = "test")]
#[post("/packages/add", data = "<input>")]
pub async fn package_add(
    db: &State<DatabaseConnection>,
    input: Json<AddBody>,
    tx: &State<Sender<Action>>,
) -> Result<(), NotFound<String>> {
    let db = db as &DatabaseConnection;

    let pkt_model = match Packages::find()
        .filter(packages::Column::Name.eq(input.name.clone()))
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
    {
        None => {
            let new_package = packages::ActiveModel {
                name: Set(input.name.clone()),
                ..Default::default()
            };

            new_package.save(db).await.expect("TODO: panic message")
        }
        Some(p) => p.into(),
    };

    let pkg = get_info_by_name(input.name.clone().as_str())
        .await
        .map_err(|_| NotFound("couldn't download package metadata".to_string()))?;

    let version_model = match Versions::find()
        .filter(versions::Column::Version.eq(pkg.version.clone()))
        .one(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?
    {
        None => {
            let new_version = versions::ActiveModel {
                version: Set(pkg.version.clone()),
                package_id: Set(pkt_model.id.clone().unwrap()),
                ..Default::default()
            };

            new_version.save(db).await.expect("TODO: panic message")
        }
        Some(p) => {
            // todo add check if this version was successfully built
            // if not allow build
            if input.force_build {
                p.into()
            } else {
                return Err(NotFound("Version already existing".to_string()));
            }
        }
    };

    let _ = tx.send(Action::Build(
        pkg.name,
        pkg.version,
        pkg.url_path.unwrap(),
        version_model,
    ));

    Ok(())
}
