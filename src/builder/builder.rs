use crate::builder::types::Action;
use crate::db::packages;
use crate::db::prelude::Packages;
use crate::repo::repo::add_pkg;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use tokio::sync::broadcast::Sender;

pub async fn init(db: DatabaseConnection, tx: Sender<Action>) {
    loop {
        if let Ok(_result) = tx.subscribe().recv().await {
            match _result {
                // add a package to parallel build
                Action::Build(name, version, url, id) => {
                    let db = db.clone();
                    tokio::spawn(async move {
                        match add_pkg(url, version, name).await {
                            Ok(_) => {
                                println!("successfully built package");

                                let mut pkg: packages::ActiveModel = Packages::find_by_id(id)
                                    .one(&db)
                                    .await
                                    .unwrap()
                                    .unwrap()
                                    .into();

                                pkg.status = Set(2);
                                let pkg: packages::Model = pkg.update(&db).await.unwrap();
                            }
                            Err(e) => {
                                println!("Error: {e}")
                            }
                        }
                    });
                }
            }
        }
    }
}
