use crate::builder::types::Action;
use crate::db::builds::ActiveModel;
use crate::db::prelude::{Builds, Packages};
use crate::db::{builds, packages, versions};
use crate::repo::repo::add_pkg;
use anyhow::anyhow;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use std::collections::HashMap;
use std::ops::Add;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::{broadcast, Mutex, Semaphore};
use tokio::task::JoinHandle;

pub async fn init(db: DatabaseConnection, tx: Sender<Action>) {
    let semaphore = Arc::new(Semaphore::new(1));
    let job_handles: Arc<Mutex<HashMap<i32, JoinHandle<_>>>> = Arc::new(Mutex::new(HashMap::new()));

    loop {
        if let Ok(_result) = tx.subscribe().recv().await {
            match _result {
                // add a package to parallel build
                Action::Build(name, version, url, version_model, build_model) => {
                    let _ = queue_package(
                        name,
                        version,
                        url,
                        version_model,
                        build_model,
                        db.clone(),
                        semaphore.clone(),
                        job_handles.clone(),
                    )
                    .await;
                }
                Action::Cancel(build_id) => {
                    let _ = cancel_build(build_id, job_handles.clone(), db.clone()).await;
                }
            }
        }
    }
}

async fn cancel_build(
    build_id: i32,
    job_handles: Arc<Mutex<HashMap<i32, JoinHandle<()>>>>,
    db: DatabaseConnection,
) -> anyhow::Result<()> {
    let build = Builds::find_by_id(build_id)
        .one(&db)
        .await?
        .ok_or(anyhow!("No build found"))?;

    let mut build: builds::ActiveModel = build.into();
    build.status = Set(Some(4));
    build.end_time = Set(Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32,
    ));
    let _ = build.clone().update(&db).await;

    job_handles
        .lock()
        .await
        .remove(&build.id.clone().unwrap())
        .ok_or(anyhow!("No build found"))?
        .abort();
    Ok(())
}

async fn queue_package(
    name: String,
    version: String,
    url: String,
    version_model: versions::ActiveModel,
    mut build_model: builds::ActiveModel,
    db: DatabaseConnection,
    semaphore: Arc<Semaphore>,
    job_handles: Arc<Mutex<HashMap<i32, JoinHandle<()>>>>,
) -> anyhow::Result<()> {
    let permits = Arc::clone(&semaphore);
    let build_id = build_model.id.clone().unwrap();

    // spawn new thread for each pkg build
    // todo add queue and build two packages in parallel
    let handle = tokio::spawn(async move {
        let _permit = permits.acquire().await.unwrap();

        // set build status to building
        build_model.status = Set(Some(0));
        build_model = build_model.save(&db).await.unwrap();

        let _ = build_package(build_model, db, version_model, version, name, url).await;
    });
    job_handles.lock().await.insert(build_id, handle);
    Ok(())
}

async fn build_package(
    mut new_build: builds::ActiveModel,
    db: DatabaseConnection,
    mut version_model: versions::ActiveModel,
    version: String,
    name: String,
    url: String,
) -> anyhow::Result<()> {
    let (tx, rx) = broadcast::channel::<String>(3);
    spawn_log_appender(db.clone(), new_build.clone(), rx);

    let package_id = version_model.package_id.clone().unwrap();
    let mut pkg: packages::ActiveModel = Packages::find_by_id(package_id)
        .one(&db)
        .await?
        .ok_or(anyhow!("no package with id {package_id} found"))?
        .into();

    // update status to building
    pkg.status = Set(0);
    pkg = pkg.update(&db).await?.into();

    match add_pkg(url, version, name, tx, false).await {
        Ok(pkg_file_name) => {
            println!("successfully built package");
            // update package success status
            pkg.status = Set(1);
            pkg.latest_version_id = Set(Some(version_model.id.clone().unwrap()));
            pkg.out_of_date = Set(false as i32);
            pkg.update(&db).await?;

            version_model.file_name = Set(Some(pkg_file_name));
            let _ = version_model.update(&db).await;

            new_build.status = Set(Some(1));
            new_build.end_time = Set(Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as u32,
            ));
            let _ = new_build.update(&db).await;
        }
        Err(e) => {
            pkg.status = Set(2);
            pkg.latest_version_id = Set(Some(version_model.id.clone().unwrap()));
            pkg.update(&db).await?;

            let _ = version_model.update(&db).await;

            new_build.status = Set(Some(2));
            new_build.end_time = Set(Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as u32,
            ));
            let _ = new_build.update(&db).await;

            println!("Error: {e}")
        }
    };
    Ok(())
}

fn spawn_log_appender(db2: DatabaseConnection, new_build2: ActiveModel, mut rx: Receiver<String>) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(output_line) => {
                    println!("{output_line}");

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
