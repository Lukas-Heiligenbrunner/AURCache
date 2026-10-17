use crate::helpers::collation::warn_on_stale_collations;
use crate::helpers::dbtype::database_type;
use crate::migration::Migrator;
use anyhow::{anyhow, bail};
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend};
use sea_orm_migration::MigratorTrait;
use std::time::Duration;
use std::{env, fs};
use tracing::log::LevelFilter;

/// Read a required Postgres env var, with a message naming it.
fn required_env(name: &str) -> anyhow::Result<String> {
    env::var(name).map_err(|_| anyhow!("No {name} envvar specified"))
}

/// Percent-encode a URL userinfo component (username or password).
///
/// Only unreserved characters pass through: everything else — `@` and `:`
/// which structure the URL, `%` which starts an escape, `/`, `?`, `#`, and
/// any non-ASCII byte — becomes `%XX`. Over-encoding the sub-delims is
/// harmless in userinfo and keeps the allowlist to what cannot break.
fn encode_userinfo(s: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if UNRESERVED.contains(&byte) {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(out, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    out
}

pub async fn init_db() -> anyhow::Result<DatabaseConnection> {
    let db: DatabaseConnection = match database_type() {
        DbBackend::Sqlite => {
            fs::create_dir_all("./db")?;

            let db_name = env::var("DB_NAME").unwrap_or_else(|_| "db.sqlite".to_string());

            let mut conn_opts = ConnectOptions::new(format!("sqlite://db/{db_name}?mode=rwc"));
            conn_opts
                .max_connections(1)
                .min_connections(1)
                .acquire_timeout(Duration::from_secs(30))
                .sqlx_logging_level(LevelFilter::Trace);
            let db = Database::connect(conn_opts).await?;
            db.execute_unprepared("
                PRAGMA foreign_keys = ON;           -- SQLite ignores every FK in the schema without this
                PRAGMA journal_mode = WAL;          -- read/write concurrency; persistent on the db file
                PRAGMA synchronous = NORMAL;        -- fsync at WAL checkpoint, not every write
                PRAGMA busy_timeout = 5000;         -- wait up to 5s for the write lock before SQLITE_BUSY
                PRAGMA wal_autocheckpoint = 1000;   -- checkpoint every 1000 pages (~1MB WAL)
                PRAGMA wal_checkpoint(TRUNCATE);    -- truncate any massive WAL left by a previous run
            ").await?;
            db
        }
        DbBackend::Postgres => {
            let db_user = required_env("DB_USER")?;
            let db_pwd = required_env("DB_PWD")?;
            let db_host = required_env("DB_HOST")?;
            let db_name = env::var("DB_NAME").unwrap_or_else(|_| "postgres".to_string());

            // Encoded, never raw: a password containing `@`, `:` or `%`
            // would corrupt a formatted URL or fail to parse. Local helper
            // rather than a crate: it is fifteen lines with a test, and the
            // only alternative here is a new dependency for one call.
            let conn_str = format!(
                "postgres://{}:{}@{db_host}/{db_name}",
                encode_userinfo(&db_user),
                encode_userinfo(&db_pwd)
            );
            let mut conn_opts = ConnectOptions::new(conn_str);
            conn_opts.sqlx_logging_level(LevelFilter::Trace);
            let db = Database::connect(conn_opts).await?;
            warn_on_stale_collations(&db).await;
            db
        }
        _ => bail!("Unsupported database type"),
    };

    Migrator::up(&db, None).await?;
    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::encode_userinfo;

    /// The characters that structure a URL must not survive into userinfo:
    /// `@` would start a host, `:` a port, `%` an escape.
    #[test]
    fn userinfo_encoding_escapes_url_structure() {
        assert_eq!(encode_userinfo("user"), "user");
        assert_eq!(encode_userinfo("p@ss:w%rd"), "p%40ss%3Aw%25rd");
        assert_eq!(encode_userinfo("a/b?c#d"), "a%2Fb%3Fc%23d");
    }
}
