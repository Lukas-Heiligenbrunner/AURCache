use crate::db::helpers::dbtype::{database_type, DbType};
use crate::db::migration::Migrator;
use anyhow::anyhow;
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use std::time::Duration;
use std::{env, fs};

pub async fn init_db() -> anyhow::Result<DatabaseConnection> {
    let db: DatabaseConnection = match database_type() {
        DbType::Sqlite => {
            // create folder for db stuff
            if fs::metadata("./db").is_err() {
                fs::create_dir("./db")?;
            }

            let mut conn_opts = ConnectOptions::new("sqlite://db/db.sqlite?mode=rwc");
            conn_opts
                .min_connections(5)
                .max_connections(100)
                .connect_timeout(Duration::from_secs(10))
                .acquire_timeout(Duration::from_secs(10))
                .max_lifetime(Duration::from_secs(10));
            Database::connect(conn_opts).await?
        }
        DbType::Postgres => {
            let db_user = env::var("DB_USER")
                .map_err(|_| anyhow!("No DB_USER envvar for POSTGRES Username specified"))?;
            let db_pwd = env::var("DB_PWD")
                .map_err(|_| anyhow!("No DB_PWD envvar for POSTGRES Password specified"))?;
            let db_host = env::var("DB_HOST")
                .map_err(|_| anyhow!("No DB_HOST envvar for POSTGRES HOST specified"))?;

            Database::connect(format!(
                "postgres://{db_user}:{db_pwd}@{db_host}/postgres?currentSchema=public"
            ))
            .await?
        }
    };

    Migrator::up(&db, None).await?;
    Ok(db)
}