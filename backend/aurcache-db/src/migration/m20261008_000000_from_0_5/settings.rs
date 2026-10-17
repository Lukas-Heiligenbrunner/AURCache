//! Settings conversions of a 0.5.0 database.

use aurcache_common::schedule::{Schedule, from_seconds_syntax};
use aurcache_common::settings::RETIRED_SETTING_KEYS;
use sea_orm::{ConnectionTrait, FromQueryResult};
use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum Settings {
    Table,
    Id,
    Key,
    Value,
}

/// 0.5.0's key for the auto-update schedule, which was an interval then.
const INTERVAL_KEY: &str = "auto_update_interval";

/// Remove the settings of builds the server no longer runs itself.
pub(super) async fn retire(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .exec_stmt(
            Query::delete()
                .from_table(Settings::Table)
                .and_where(Expr::col(Settings::Key).is_in(RETIRED_SETTING_KEYS.iter().copied()))
                .to_owned(),
        )
        .await
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
