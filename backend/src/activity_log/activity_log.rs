use crate::activity_log::activity_serializer::ActivitySerializer;
use crate::activity_log::package_add_activity::PackageAddActivity;
use crate::db;
use crate::db::activities;
use crate::db::activities::ActivityType;
use crate::db::prelude::Activities;
use anyhow::anyhow;
use rocket::serde::{Deserialize, Serialize};
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, EntityTrait, FromQueryResult, Order, QueryOrder,
    QuerySelect,
};
use std::time::{SystemTime, UNIX_EPOCH};
use utoipa::ToSchema;

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize)]
pub struct Activity {
    pub timestamp: i64,
    pub text: String,
    pub user: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ActivityLog {
    db: DatabaseConnection,
}

impl ActivityLog {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn add<T: Serialize + ActivitySerializer>(
        &self,
        activity: T,
        user: Option<String>,
    ) -> anyhow::Result<()> {
        let activity = serde_json::to_string(&activity)?;
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

        activities::ActiveModel {
            timestamp: Set(timestamp),
            data: Set(activity),
            user: Set(user),
            typ: Set(ActivityType::AddPackage),
            ..std::default::Default::default()
        }
        .save(&self.db)
        .await
        .map_err(|e| anyhow!(e.to_string()))?;
        Ok(())
    }

    pub async fn list(&self, limit: Option<u64>) -> anyhow::Result<Vec<Activity>> {
        // List activities from database
        let activities = Activities::find()
            .order_by(activities::Column::Timestamp, Order::Desc)
            .limit(limit.unwrap_or(10))
            .into_model::<db::activities::Model>()
            .all(&self.db)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;

        let t: Vec<Activity> = activities
            .iter()
            .map(|x| {
                let v: Box<dyn ActivitySerializer> = match x.typ {
                    ActivityType::AddPackage => Box::from(
                        serde_json::from_str::<PackageAddActivity>(x.data.as_str()).unwrap(),
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
                    text: v.format(),
                    user: x.user.clone(),
                }
            })
            .collect();
        Ok(t)
    }
}
