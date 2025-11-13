use crate::activity_serializer::ActivitySerializer;
use crate::package_add_activity::PackageAddActivity;
use crate::package_delete_activity::PackageDeleteActivity;
use crate::package_update_activity::PackageUpdateActivity;
use anyhow::anyhow;
use aurcache_db::activities;
use aurcache_db::activities::ActivityType;
use aurcache_db::prelude::Activities;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, EntityTrait, FromQueryResult, Order, QueryOrder,
    QuerySelect,
};
use serde::{Deserialize, Serialize};
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
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn add<T: Serialize + ActivitySerializer>(
        &self,
        activity: T,
        activity_type: ActivityType,
        user: Option<String>,
    ) -> anyhow::Result<()> {
        let activity = serde_json::to_string(&activity)?;
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

        activities::ActiveModel {
            timestamp: Set(timestamp),
            data: Set(activity),
            user: Set(user),
            typ: Set(activity_type),
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
            .into_model::<activities::Model>()
            .all(&self.db)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;

        let t: Vec<Activity> = activities
            .iter()
            .filter_map(|x| {
                if let Ok(v) = self.deserialize_type(x.typ, &x.data) {
                    Some(Activity {
                        timestamp: x.timestamp,
                        text: v.format(),
                        user: x.user.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();
        Ok(t)
    }

    fn deserialize_type(
        &self,
        activity_type: ActivityType,
        data: &str,
    ) -> anyhow::Result<Box<dyn ActivitySerializer>> {
        match activity_type {
            ActivityType::AddPackage => {
                Ok(Box::from(serde_json::from_str::<PackageAddActivity>(data)?))
            }
            ActivityType::RemovePackage => Ok(Box::from(serde_json::from_str::<
                PackageDeleteActivity,
            >(data)?)),
            ActivityType::UpdatePackage => Ok(Box::from(serde_json::from_str::<
                PackageUpdateActivity,
            >(data)?)),
            ActivityType::StartBuild => todo!("StartBuild"),
            ActivityType::FinishBuild => todo!("FinishBuild"),
        }
    }
}
