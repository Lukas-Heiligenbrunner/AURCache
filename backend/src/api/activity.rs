use crate::activity_log::activity_serializer::ActivitySerializer;
use crate::activity_log::package_add_activity::PackageAdd;
use crate::api::types::authenticated::Authenticated;
use crate::api::types::input::Activity;
use crate::db;
use crate::db::activities::ActivityType;
use crate::db::migration::Order;
use crate::db::prelude::Activities;
use crate::db::activities;
use rocket::response::status::NotFound;
use rocket::serde::json::Json;
use rocket::{get, State};
use sea_orm::{DatabaseConnection, EntityTrait, QueryOrder, QuerySelect};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(activity))]
pub struct ActivityApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Get last n Activity entries", body = [Vec<Activity>]),
    )
)]
#[get("/activity")]
pub async fn activity(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<Vec<Activity>>, NotFound<String>> {
    let db = db as &DatabaseConnection;

    let activities = Activities::find()
        .order_by(activities::Column::Timestamp, Order::Desc)
        .limit(10)
        .into_model::<db::activities::Model>()
        .all(db)
        .await
        .map_err(|e| NotFound(e.to_string()))?;

    let activities = activities
        .iter()
        .map(|x| {
            let v: Box<dyn ActivitySerializer> = match x.typ {
                ActivityType::AddPackage => Box::from(
                    serde_json::from_str::<PackageAdd>(x.data.as_str()).unwrap() as PackageAdd,
                ),
                ActivityType::RemovePackage => {
                    todo!("RemovePackage")
                }
                ActivityType::UpdatePackage => {
                    todo!("UpdatePackage")
                }
                ActivityType::StartBuild => {
                    todo!("StartBuild")
                }
                ActivityType::FinishBuild => {
                    todo!("FinishBuild")
                }
            };

            Activity {
                timestamp: x.timestamp,
                text: v.serialize(),
                user: None,
            }
        })
        .collect();

    Ok(Json(activities))
}
