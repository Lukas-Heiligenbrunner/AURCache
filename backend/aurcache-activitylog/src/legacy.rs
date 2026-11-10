//! Bringing the curated activity log's rows into the structured log.
//!
//! 0.5.0 kept a hand-curated activity log: a few kinds, each with a JSON
//! payload of its own. Every one it wrote is now an [`Event`] variant, so its
//! rows are converted once, at startup, with their original time and actor,
//! and removed from the old table as they land in the new one.
//! A row that cannot be converted stays where it is and is reported, rather
//! than taking the rest with it.

use crate::activity_utils::{LogRecord, write};
use crate::events::{Event, QueueCause};
use aurcache_common::api::log::PackageRef;
use aurcache_db::activities::{self, ActivityType};
use aurcache_db::prelude::Activities;
use sea_orm::{DatabaseConnection, EntityTrait, QueryOrder, TransactionTrait};
use serde::Deserialize;

/// Move every convertible activity row into the structured log, returning how
/// many moved.
///
/// Cheap when there is nothing to do, so it runs on every start rather than
/// being tracked as done.
///
/// # Errors
///
/// Returns a database error from reading the old rows; a row that fails to
/// convert or to write is logged and left in place.
pub async fn import_activities(db: &DatabaseConnection) -> anyhow::Result<u64> {
    let rows = Activities::find()
        .order_by_asc(activities::Column::Id)
        .all(db)
        .await?;
    if rows.is_empty() {
        return Ok(0);
    }

    let mut moved = 0;
    for row in rows {
        // The old log credited the auto-updater to a user called "Server"; the
        // server acting on its own is an entry with no user.
        let user = row.user.clone().filter(|user| user != SERVER_USER);
        let Some(event) = convert(row.typ, &row.data, user.is_none()) else {
            tracing::warn!(
                "activity row {} ({:?}) could not be converted; leaving it in place",
                row.id,
                row.typ
            );
            continue;
        };
        let record = LogRecord::of(&event, None, user, row.timestamp)?;
        // The new entry and removal of the old one commit together, so a
        // failed delete or interrupted import cannot duplicate the event.
        let imported = db
            .transaction::<_, (), anyhow::Error>(|txn| {
                Box::pin(async move {
                    write(txn, record).await?;
                    Activities::delete_by_id(row.id).exec(txn).await?;
                    Ok(())
                })
            })
            .await;
        if let Err(e) = imported {
            tracing::warn!("could not import activity row {}: {e}", row.id);
            continue;
        }
        moved += 1;
    }
    Ok(moved)
}

#[derive(Deserialize)]
struct Package {
    package: String,
}

#[derive(Deserialize)]
struct Update {
    package: String,
    forced: bool,
}

/// The name the old log gave the server when it acted on its own.
const SERVER_USER: &str = "Server";

/// The event an activity row recorded, in the vocabulary it is now written in.
///
/// `by_server` is whether nobody asked for it, which decides what a forced
/// update was: the auto-updater forced every VCS package past the version
/// check, and that was an update; a person forcing one was rebuilding it.
/// Which of a rebuild and a retry it was is not recorded, and rebuild is the
/// likelier.
fn convert(typ: ActivityType, data: &str, by_server: bool) -> Option<Event> {
    fn parse<'a, T: Deserialize<'a>>(data: &'a str) -> Option<T> {
        serde_json::from_str(data).ok()
    }
    let pkg = |name: String| PackageRef::from(name);
    Some(match typ {
        ActivityType::AddPackage => Event::PackageAdded {
            pkg: pkg(parse::<Package>(data)?.package),
        },
        ActivityType::RemovePackage => Event::PackageDeleted {
            pkg: pkg(parse::<Package>(data)?.package),
        },
        ActivityType::UpdatePackage => {
            let update = parse::<Update>(data)?;
            Event::BuildQueued {
                pkg: pkg(update.package),
                cause: if update.forced && !by_server {
                    QueueCause::Rebuild
                } else {
                    QueueCause::Update
                },
                builds: vec![],
                version: None,
                retried: None,
                needed_by: None,
            }
        }
        // Declared, never written.
        ActivityType::StartBuild | ActivityType::FinishBuild => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::ActiveValue::Set;
    use sea_orm::{ActiveModelTrait, PaginatorTrait};

    async fn old_row(db: &DatabaseConnection, typ: ActivityType, data: &str, user: Option<&str>) {
        activities::ActiveModel {
            typ: Set(typ),
            data: Set(data.to_string()),
            user: Set(user.map(str::to_string)),
            timestamp: Set(1_700_000_000),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    /// Every kind the activity log wrote comes across as its event, keeping
    /// when it happened and who did it, and leaves the old table empty.
    #[tokio::test]
    async fn every_written_kind_moves_with_its_time_and_actor() {
        let db = aurcache_db::testing::memory_db().await;
        for (typ, data) in [
            (ActivityType::AddPackage, r#"{"package":"hello"}"#),
            (ActivityType::RemovePackage, r#"{"package":"hello"}"#),
            (
                ActivityType::UpdatePackage,
                r#"{"package":"hello","forced":true}"#,
            ),
        ] {
            old_row(&db, typ, data, Some("alice")).await;
        }

        assert_eq!(import_activities(&db).await.unwrap(), 3);
        assert_eq!(Activities::find().count(&db).await.unwrap(), 0);

        let entries = aurcache_db::prelude::Logs::find().all(&db).await.unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.timestamp == 1_700_000_000));
        assert!(entries.iter().all(|e| e.user.as_deref() == Some("alice")));
    }

    /// A forced update from the auto-updater was an update, and it becomes the
    /// server's own entry; one a person forced was a rebuild, and stays theirs.
    #[tokio::test]
    async fn a_forced_update_is_an_update_or_a_rebuild_by_who_asked() {
        let db = aurcache_db::testing::memory_db().await;
        let forced = r#"{"package":"hello","forced":true}"#;
        old_row(&db, ActivityType::UpdatePackage, forced, Some("Server")).await;
        old_row(&db, ActivityType::UpdatePackage, forced, Some("alice")).await;
        assert_eq!(import_activities(&db).await.unwrap(), 2);

        let entries = aurcache_db::prelude::Logs::find().all(&db).await.unwrap();
        let by = |user: Option<&str>| {
            entries
                .iter()
                .find(|e| e.user.as_deref() == user)
                .map(|e| e.message.as_str())
        };
        assert_eq!(by(None), Some("queued a build of hello (update)"));
        assert_eq!(by(Some("alice")), Some("queued a build of hello (rebuild)"));
        assert!(entries.iter().all(|e| e.kind == "build.queued"));
    }

    /// A row that does not parse stays in the old table for someone to look
    /// at, and does not stop the rest.
    #[tokio::test]
    async fn an_unreadable_row_stays_behind() {
        let db = aurcache_db::testing::memory_db().await;
        old_row(&db, ActivityType::AddPackage, "not json", None).await;
        old_row(
            &db,
            ActivityType::AddPackage,
            r#"{"package":"hello"}"#,
            None,
        )
        .await;
        assert_eq!(import_activities(&db).await.unwrap(), 1);
        assert_eq!(Activities::find().count(&db).await.unwrap(), 1);
    }

    /// Nothing to import is nothing to do, on every start after the first.
    #[tokio::test]
    async fn an_empty_table_imports_nothing() {
        let db = aurcache_db::testing::memory_db().await;
        assert_eq!(import_activities(&db).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_failed_delete_does_not_duplicate_an_imported_event() {
        use sea_orm::ConnectionTrait;

        let db = aurcache_db::testing::memory_db().await;
        old_row(
            &db,
            ActivityType::AddPackage,
            r#"{"package":"hello"}"#,
            Some("alice"),
        )
        .await;
        db.execute_unprepared(
            "CREATE TRIGGER refuse_activity_delete BEFORE DELETE ON activity \
             BEGIN SELECT RAISE(FAIL, 'cannot delete activity'); END",
        )
        .await
        .unwrap();

        assert_eq!(import_activities(&db).await.unwrap(), 0);
        assert_eq!(
            aurcache_db::prelude::Logs::find().count(&db).await.unwrap(),
            0
        );
        assert_eq!(
            aurcache_db::prelude::LogEntities::find()
                .count(&db)
                .await
                .unwrap(),
            0
        );
        assert_eq!(Activities::find().count(&db).await.unwrap(), 1);

        db.execute_unprepared("DROP TRIGGER refuse_activity_delete")
            .await
            .unwrap();
        assert_eq!(import_activities(&db).await.unwrap(), 1);
        assert_eq!(import_activities(&db).await.unwrap(), 0);
        assert_eq!(
            aurcache_db::prelude::Logs::find().count(&db).await.unwrap(),
            1
        );
    }
}
