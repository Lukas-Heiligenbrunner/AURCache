use crate::builder::types::Action;
use crate::repo::repo::add_pkg;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use tokio::sync::broadcast::Sender;

pub async fn init(db: DatabaseConnection, tx: Sender<Action>) {
    loop {
        if let Ok(_result) = tx.subscribe().recv().await {
            match _result {
                // add a package to parallel build
                Action::Build(name, version, url, mut version_model) => {
                    let db = db.clone();

                    // spawn new thread for each pkg build
                    tokio::spawn(async move {
                        match add_pkg(url, version, name).await {
                            Ok(pkg_file_name) => {
                                println!("successfully built package");

                                // update status
                                version_model.status = Set(Some(1));
                                version_model.file_name = Set(Some(pkg_file_name));
                                version_model.update(&db).await.unwrap();
                            }
                            Err(e) => {
                                version_model.status = Set(Some(2));
                                version_model.update(&db).await.unwrap();

                                println!("Error: {e}")
                            }
                        }
                    });
                }
            }
        }
    }
}
