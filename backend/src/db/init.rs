use crate::db::helpers::dbtype::{database_type, DbType};
use crate::db::migration::Migrator;
use anyhow::anyhow;
use sea_orm::{ConnectionTrait, ConnectOptions, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use std::{env, fs};
use rocket::log::private::LevelFilter;

pub async fn init_db() -> anyhow::Result<DatabaseConnection> {
    let db: DatabaseConnection = match database_type() {
        DbType::Sqlite => {
            // create folder for db stuff
            if fs::metadata("./db").is_err() {
                fs::create_dir("./db")?;
            }

            let mut conn_opts = ConnectOptions::new("sqlite://db/db.sqlite?mode=rwc");
            conn_opts
                .max_connections(100)
                .sqlx_logging_level(LevelFilter::Info);
            let dbb = Database::connect(conn_opts).await?;
            dbb.execute_unprepared("
                PRAGMA journal_mode = WAL;          -- better write-concurrency
                PRAGMA synchronous = NORMAL;        -- fsync only in critical moments
                PRAGMA wal_autocheckpoint = 1000;   -- write WAL changes back every 1000 pages, for an in average 1MB WAL file. May affect readers if number is increased
                PRAGMA wal_checkpoint(TRUNCATE);    -- free some space by truncating possibly massive WAL files from the last run.
            ").await?;
            dbb
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
