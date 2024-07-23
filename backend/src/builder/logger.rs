use crate::db::builds;
use crate::db::builds::ActiveModel;
use crate::db::prelude::Builds;
use anyhow::anyhow;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use std::ops::Add;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Receiver;

pub(crate) fn spawn_log_appender(
    db2: DatabaseConnection,
    new_build2: ActiveModel,
    mut rx: Receiver<String>,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(output_line) => {
                    print!("{}", output_line);

                    let _ = append_db_log_output(&db2, output_line, new_build2.id.clone().unwrap())
                        .await;
                }
                Err(e) => match e {
                    RecvError::Closed => {
                        break;
                    }
                    RecvError::Lagged(_) => {}
                },
            }
        }
    });
}

async fn append_db_log_output(
    db: &DatabaseConnection,
    text: String,
    build_id: i32,
) -> anyhow::Result<()> {
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await?
        .ok_or(anyhow!("build not found"))?;

    let mut build: builds::ActiveModel = build.into();

    match build.output.unwrap() {
        None => {
            build.output = Set(Some(text.add("\n")));
        }
        Some(s) => {
            build.output = Set(Some(s.add(text.as_str()).add("\n")));
        }
    }

    build.update(db).await?;
    Ok(())
}
