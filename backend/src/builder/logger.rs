use crate::db::builds;
use crate::db::prelude::Builds;
use anyhow::anyhow;
use log::{debug, error};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set, TransactionTrait};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tokio::time;

#[derive(Debug, Clone)]
pub struct BuildLogger {
    build_id: i32,
    db: DatabaseConnection,
    buffer: Arc<Mutex<Vec<String>>>,
    notifier: Arc<Notify>,
}

impl BuildLogger {
    pub fn new(build_id: i32, db: DatabaseConnection) -> Self {
        let logger = Self {
            build_id,
            db,
            buffer: Arc::new(Mutex::new(Vec::new())),
            notifier: Arc::new(Notify::new()),
        };

        // Start a background task for flushing
        let buffer_clone = Arc::clone(&logger.buffer);
        let notifier_clone = Arc::clone(&logger.notifier);
        let db_clone = logger.db.clone();
        let build_id = logger.build_id;

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(2));
            loop {
                interval.tick().await;
                notifier_clone.notified().await; // Wait for a notification or timer
                if let Err(e) = Self::flush_buffer(&db_clone, build_id, &buffer_clone)
                    .await {
                    error!("Failed to flush log buffer for build #{}: {}", build_id, e);
                }
            }
        });

        logger
    }

    pub async fn append(&self, text: String) -> anyhow::Result<()> {
        debug!("{}", text);

        // Add the text to the buffer
        let mut buffer = self.buffer.lock().await;
        buffer.push(text);
        self.notifier.notify_one(); // Notify the background task
        Ok(())
    }

    async fn flush_buffer(
        db: &DatabaseConnection,
        build_id: i32,
        buffer: &Arc<Mutex<Vec<String>>>,
    ) -> anyhow::Result<()> {
        let mut buffer = buffer.lock().await;
        if buffer.is_empty() {
            return Ok(()); // Nothing to flush
        }

        let combined_text = buffer.join("");
        buffer.clear(); // Clear the buffer

        let txn = db.begin().await?;
        let mut build: builds::ActiveModel = Builds::find_by_id(build_id)
            .one(&txn)
            .await?
            .ok_or(anyhow!("build not found"))?
            .into();

        match build.output.unwrap() {
            None => {
                build.output = Set(Some(combined_text));
            }
            Some(s) => {
                build.output = Set(Some(format!("{s}{combined_text}")));
            }
        }

        build.update(&txn).await?;
        txn.commit().await?;
        debug!("Log buffer flushed!");
        Ok(())
    }
}

impl Drop for BuildLogger {
    fn drop(&mut self) {
        // force a flush when object is destroyed

        let db = self.db.clone();
        let buffer = Arc::clone(&self.buffer);
        let build_id = self.build_id;

        tokio::spawn(async move {
            Self::flush_buffer(&db, build_id, &buffer).await
        });
    }
}