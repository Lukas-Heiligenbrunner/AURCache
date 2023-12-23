use crate::aur::aur::get_info_by_name;
use crate::builder::types::Action;
use crate::db::{packages, versions};
use rocket::serde::json::Json;
use rocket::serde::Deserialize;
use rocket::{post, State};
use rocket_okapi::okapi::schemars;
use rocket_okapi::{openapi, JsonSchema};
use sea_orm::ActiveModelTrait;
use sea_orm::{DatabaseConnection, Set};
use tokio::sync::broadcast::Sender;

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct AddBody {
    name: String,
}

#[openapi(tag = "test")]
#[post("/packages/add", data = "<input>")]
pub async fn package_add(
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
        ..Default::default()
    };

    let pkt_model = new_package.save(db).await.expect("TODO: panic message");

    let new_version = versions::ActiveModel {
        version: Set(pkg.version.clone()),
        package_id: Set(pkt_model.id.clone().unwrap()),
        ..Default::default()
    };

    let version_model = new_version.save(db).await.expect("TODO: panic message");

    let _ = tx.send(Action::Build(
        pkg.name,
        pkg.version,
        pkg.url_path.unwrap(),
        version_model,
    ));

    Ok(())
}
