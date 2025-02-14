use crate::db::helpers::dbtype::database_type;
use crate::db::migration::Migrator;
use anyhow::{anyhow, bail};
use rocket::log::private::LevelFilter;
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend};
use sea_orm_migration::MigratorTrait;
use std::{env, fs};

pub async fn init_db() -> anyhow::Result<DatabaseConnection> {
    let db: DatabaseConnection = match database_type() {
        DbBackend::Sqlite => {
            if fs::metadata("./db").is_err() {
                fs::create_dir("./db")?;
            }

            let db_name = env::var("DB_NAME").unwrap_or("db.sqlite".to_string());

            let mut conn_opts = ConnectOptions::new(format!("sqlite://db/{}?mode=rwc", db_name));
            conn_opts
                .max_connections(100)
                .sqlx_logging_level(LevelFilter::Trace);
            let db = Database::connect(conn_opts).await?;
            db.execute_unprepared("
                PRAGMA journal_mode = WAL;          -- better write-concurrency
                PRAGMA synchronous = NORMAL;        -- fsync only in critical moments
                PRAGMA wal_autocheckpoint = 1000;   -- write WAL changes back every 1000 pages, for an in average 1MB WAL file. May affect readers if number is increased
                PRAGMA wal_checkpoint(TRUNCATE);    -- free some space by truncating possibly massive WAL files from the last run.
            ").await?;
            db
        }
        DbBackend::Postgres => {
            let db_user = env::var("DB_USER")
                .map_err(|_| anyhow!("No DB_USER envvar for POSTGRES Username specified"))?;
            let db_pwd = env::var("DB_PWD")
                .map_err(|_| anyhow!("No DB_PWD envvar for POSTGRES Password specified"))?;
            let db_host = env::var("DB_HOST")
                .map_err(|_| anyhow!("No DB_HOST envvar for POSTGRES HOST specified"))?;
            let db_name = env::var("DB_NAME").unwrap_or("postgres".to_string());

            let conn_str = format!("postgres://{db_user}:{db_pwd}@{db_host}/{db_name}");
            let mut conn_opts = ConnectOptions::new(conn_str);
            conn_opts.sqlx_logging_level(LevelFilter::Trace);
            Database::connect(conn_opts).await?
        }
        _ => bail!("Unsupported database type"),
    };

    Migrator::up(&db, None).await?;
    Ok(db)
}
