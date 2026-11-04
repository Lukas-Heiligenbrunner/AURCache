//! Noticing when Postgres's text ordering changed under an existing database.
//!
//! Postgres sorts text with the operating system's C library (or ICU), and
//! records the library's collation version when a database is created. When
//! that library changes under existing data -- typically a `postgres:17` image
//! tag that moved to a newer Debian release -- indexes on text were built in an
//! order the server no longer agrees with. A lookup can then miss a row that is
//! there, and a unique index can admit a duplicate.
//!
//! Postgres already warns, but on every new connection, with a hint that does
//! not say which commands to run. This checks once at startup and says so
//! plainly. It does not fix anything: rebuilding indexes takes locks and needs
//! the database's owner, which is the operator's call, and only refreshing the
//! recorded version would silence the warning without repairing anything.
//!
//! SQLite has nothing to check. Its collations (`BINARY`, `NOCASE`, `RTRIM`)
//! are part of SQLite itself rather than the operating system, so a database
//! file sorts the same wherever it is opened.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use tracing::{debug, warn};

/// A database whose recorded collation version is not the one the server now
/// provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleCollation {
    pub database: String,
    pub recorded: String,
    pub actual: String,
}

impl StaleCollation {
    /// The warning to log, with the commands that repair it.
    #[must_use]
    pub fn message(&self) -> String {
        let name = quote_ident(&self.database);
        format!(
            "Postgres database {:?} was created with collation version {}, but the server now \
             provides {}. Its operating system changed (usually a newer postgres image), so \
             indexes on text may be ordered wrongly and miss rows or admit duplicates. As the \
             database owner, connected to that database (REINDEX DATABASE only rebuilds the \
             current one), run: REINDEX DATABASE {name}; ALTER DATABASE {name} REFRESH \
             COLLATION VERSION; -- pinning the image's Debian release (e.g. postgres:17-trixie) \
             keeps this from recurring.",
            self.database, self.recorded, self.actual
        )
    }
}

/// Databases on this server whose collation version changed since it was
/// recorded.
///
/// Every database that accepts connections, not only AURCache's: `template1`
/// is what a new database is copied from, so a stale one hands the problem on.
///
/// # Errors
/// If the query fails, which it does on Postgres before 15: the per-database
/// version it compares is not recorded there.
pub async fn stale_collations(
    db: &DatabaseConnection,
) -> Result<Vec<StaleCollation>, sea_orm::DbErr> {
    let rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT datname, datcollversion, \
                    pg_database_collation_actual_version(oid) AS actual \
             FROM pg_database \
             WHERE datallowconn \
             ORDER BY datname"
                .to_string(),
        ))
        .await?;

    let mut stale = Vec::new();
    for row in rows {
        let database: String = row.try_get("", "datname")?;
        let recorded: Option<String> = row.try_get("", "datcollversion")?;
        let actual: Option<String> = row.try_get("", "actual")?;
        if let Some(found) = compare(database, recorded, actual) {
            stale.push(found);
        }
    }
    Ok(stale)
}

/// Whether one database's versions disagree.
///
/// Either side missing means there is nothing to compare: the `C` and `POSIX`
/// collations have no version, because they never change.
fn compare(
    database: String,
    recorded: Option<String>,
    actual: Option<String>,
) -> Option<StaleCollation> {
    match (recorded, actual) {
        (Some(recorded), Some(actual)) if recorded != actual => Some(StaleCollation {
            database,
            recorded,
            actual,
        }),
        _ => None,
    }
}

/// Log a warning for each database whose collation version changed.
///
/// Best effort: a server too old to record the version, or a role not allowed
/// to read `pg_database`, is logged at debug and startup carries on.
pub async fn warn_on_stale_collations(db: &DatabaseConnection) {
    if db.get_database_backend() != DbBackend::Postgres {
        return;
    }
    match stale_collations(db).await {
        Ok(stale) => {
            for database in stale {
                warn!("{}", database.message());
            }
        }
        Err(e) => debug!("could not check Postgres collation versions: {e}"),
    }
}

/// A database name as SQL, so the printed commands work when pasted even for a
/// name with capitals or punctuation.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::{StaleCollation, compare, quote_ident};

    #[test]
    fn a_changed_version_is_stale() {
        assert_eq!(
            compare(
                "postgres".to_string(),
                Some("2.36".to_string()),
                Some("2.41".to_string())
            ),
            Some(StaleCollation {
                database: "postgres".to_string(),
                recorded: "2.36".to_string(),
                actual: "2.41".to_string(),
            })
        );
    }

    #[test]
    fn an_unchanged_version_is_not() {
        assert_eq!(
            compare(
                "postgres".to_string(),
                Some("2.41".to_string()),
                Some("2.41".to_string())
            ),
            None
        );
    }

    /// `C` and `POSIX` have no version to record, and never change.
    #[test]
    fn a_missing_version_is_not_stale() {
        assert_eq!(
            compare("postgres".to_string(), None, Some("2.41".to_string())),
            None
        );
        assert_eq!(
            compare("postgres".to_string(), Some("2.36".to_string()), None),
            None
        );
        assert_eq!(compare("postgres".to_string(), None, None), None);
    }

    /// The point of the message is that it can be pasted: reindex first, since
    /// refreshing alone only hides the warning.
    #[test]
    fn the_message_names_both_versions_and_reindexes_before_refreshing() {
        let message = StaleCollation {
            database: "postgres".to_string(),
            recorded: "2.36".to_string(),
            actual: "2.41".to_string(),
        }
        .message();
        assert!(
            message.contains("2.36") && message.contains("2.41"),
            "{message}"
        );
        let reindex = message
            .find(r#"REINDEX DATABASE "postgres";"#)
            .expect(&message);
        let refresh = message
            .find(r#"ALTER DATABASE "postgres" REFRESH COLLATION VERSION;"#)
            .expect(&message);
        assert!(reindex < refresh, "{message}");
    }

    #[test]
    fn a_name_is_quoted_as_an_identifier() {
        assert_eq!(quote_ident("aurcache"), r#""aurcache""#);
        assert_eq!(quote_ident(r#"we"ird"#), r#""we""ird""#);
    }
}
