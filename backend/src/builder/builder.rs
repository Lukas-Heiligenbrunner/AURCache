use crate::builder::types::Action;
use crate::db::prelude::{Builds, Packages};
use crate::db::{builds, packages};
use crate::repo::repo::add_pkg;
use anyhow::anyhow;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use std::ops::Add;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Sender;

pub async fn init(db: DatabaseConnection, tx: Sender<Action>) {
    loop {
        if let Ok(_result) = tx.subscribe().recv().await {
            match _result {
                // add a package to parallel build
                Action::Build(name, version, url, mut version_model) => {
                    let db = db.clone();

                    let build = builds::ActiveModel {
                        pkg_id: version_model.package_id.clone(),
                        version_id: version_model.id.clone(),
                        ouput: Set(None),
                        status: Set(Some(0)),
                        ..Default::default()
                    };
                    let mut new_build = build.save(&db).await.unwrap();

                    // spawn new thread for each pkg build
                    // todo add queue and build two packages in parallel
                    tokio::spawn(async move {
                        let (tx, mut rx) = broadcast::channel::<String>(3);

                        let db2 = db.clone();
                        let new_build2 = new_build.clone();
                        tokio::spawn(async move {
                            loop {
                                match rx.recv().await {
                                    Ok(output_line) => {
                                        println!("{output_line}");

                                        let _ = append_db_log_output(
                                            &db2,
                                            output_line,
                                            new_build2.id.clone().unwrap(),
                                        )
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

                        match add_pkg(url, version, name, tx).await {
                            Ok(pkg_file_name) => {
                                println!("successfully built package");
                                let _ = set_pkg_status(
                                    &db,
                                    version_model.package_id.clone().unwrap(),
                                    1,
                                )
                                .await;

                                version_model.file_name = Set(Some(pkg_file_name));
                                let _ = version_model.update(&db).await;

                                new_build.status = Set(Some(1));
                                let _ = new_build.update(&db).await;
                            }
                            Err(e) => {
                                let _ = set_pkg_status(
                                    &db,
                                    version_model.package_id.clone().unwrap(),
                                    2,
                                )
                                .await;
                                let _ = version_model.update(&db).await;

                                new_build.status = Set(Some(2));
                                let _ = new_build.update(&db).await;

                                println!("Error: {e}")
                            }
                        }
                    });
                }
            }
        }
    }
}

// todo maybe move to helper file
async fn set_pkg_status(
    db: &DatabaseConnection,
    package_id: i32,
    status: i32,
) -> anyhow::Result<()> {
    let mut pkg: packages::ActiveModel = Packages::find_by_id(package_id)
        .one(db)
        .await?
        .ok_or(anyhow!("no package with id {package_id} found"))?
        .into();

    pkg.status = Set(status);
    pkg.update(db).await?;
    Ok(())
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

    match build.ouput.unwrap() {
        None => {
            build.ouput = Set(Some(text.add("\n")));
        }
        Some(s) => {
            build.ouput = Set(Some(s.add(text.as_str()).add("\n")));
        }
    }

    build.update(db).await?;
    Ok(())
}
