//! Settings conversions of a 0.5.0 database.

use aurcache_common::schedule::{Schedule, from_seconds_syntax};
use aurcache_common::settings::{EMBEDDED_WORKER_LIMITS, RETIRED_SETTING_KEYS};
use sea_orm::{ConnectionTrait, FromQueryResult};
use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum Settings {
    Table,
    Id,
    Key,
    Value,
    PkgId,
}

/// 0.5.0's key for the auto-update schedule, which was an interval then.
const INTERVAL_KEY: &str = "auto_update_interval";

/// Remove the settings of builds the server no longer runs itself, but for
/// the server-wide limits the hybrid image's embedded worker takes over; see
/// [`EMBEDDED_WORKER_LIMITS`]. Each one dropped is named in the log.
pub(super) async fn retire(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    #[derive(FromQueryResult)]
    struct Row {
        id: i32,
        key: String,
        value: Option<String>,
        pkg_id: i32,
    }
    let db = manager.get_connection();
    let rows = Row::find_by_statement(
        manager.get_database_backend().build(
            Query::select()
                .columns([
                    Settings::Id,
                    Settings::Key,
                    Settings::Value,
                    Settings::PkgId,
                ])
                .from(Settings::Table)
                .and_where(Expr::col(Settings::Key).is_in(RETIRED_SETTING_KEYS.iter().copied())),
        ),
    )
    .all(db)
    .await?;
    for row in rows {
        let global = crate::settings::package_of(row.pkg_id).is_none();
        if global
            && EMBEDDED_WORKER_LIMITS
                .iter()
                .any(|(old, _)| *old == row.key)
        {
            continue;
        }
        let value = row.value.as_deref().unwrap_or("");
        if global {
            tracing::warn!(
                "dropped the setting {}={value}: nothing reads it any more",
                row.key
            );
        } else {
            tracing::warn!(
                "dropped package #{}'s setting {}={value}: a build's limits are its worker's now",
                row.pkg_id,
                row.key
            );
        }
        db.execute(
            &Query::delete()
                .from_table(Settings::Table)
                .and_where(Expr::col(Settings::Id).eq(row.id))
                .to_owned(),
        )
        .await?;
    }
    Ok(())
}

/// Turn a stored auto-update interval into the crontab schedule it now is,
/// under the key it now has. A value that already parses as a schedule, or
/// is empty, is left as it is; one that converts to neither stays too, and
/// the scheduler reports it.
pub(super) async fn convert_schedule(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    #[derive(FromQueryResult)]
    struct Row {
        id: i32,
        value: Option<String>,
    }
    let db = manager.get_connection();
    let rows = Row::find_by_statement(
        manager.get_database_backend().build(
            Query::select()
                .columns([Settings::Id, Settings::Value])
                .from(Settings::Table)
                .and_where(Expr::col(Settings::Key).eq(INTERVAL_KEY)),
        ),
    )
    .all(db)
    .await?;
    for Row { id, value } in rows {
        let Some(old) = value else { continue };
        if old.trim().is_empty() || Schedule::parse(&old, 0).is_ok() {
            continue;
        }
        let Some(new) = from_seconds_syntax(&old) else {
            continue;
        };
        db.execute(
            &Query::update()
                .table(Settings::Table)
                .value(Settings::Value, new)
                .and_where(Expr::col(Settings::Id).eq(id))
                .to_owned(),
        )
        .await?;
    }
    manager
        .exec_stmt(
            Query::update()
                .table(Settings::Table)
                .value(Settings::Key, "auto_update_schedule")
                .and_where(Expr::col(Settings::Key).eq(INTERVAL_KEY))
                .to_owned(),
        )
        .await
}
