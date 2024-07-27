use crate::db::builds;
use crate::db::prelude::Builds;
use anyhow::anyhow;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use std::ops::Add;

#[derive(Debug, Clone)]
pub struct BuildLogger {
    build_id: i32,
    db: DatabaseConnection,
}

impl BuildLogger {
    pub fn new(build_id: i32, db: DatabaseConnection) -> Self {
        Self { build_id, db }
    }

    pub async fn append(&self, mut text: String) -> anyhow::Result<()> {
        let mut build: builds::ActiveModel = Builds::find_by_id(self.build_id)
            .one(&self.db)
            .await?
            .ok_or(anyhow!("build not found"))?
            .into();

        if !text.ends_with('\n') {
            text = text.add("\n");
        }

        // todo replace this with debug log
        print!("{}", text);

        match build.output.unwrap() {
            None => {
                build.output = Set(Some(text));
            }
            Some(s) => {
                build.output = Set(Some(format!("{s}{text}")));
            }
        }

        build.update(&self.db).await?;
        Ok(())
    }
}