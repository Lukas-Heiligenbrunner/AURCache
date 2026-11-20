//! Holding every package's stored build flags to the ones a package chooses.
//!
//! A package stores only offered flags, in their canonical form (see
//! [`aurcache_common::build_flags`]). A list stored before an option stopped
//! being offered still names it, and [`settle`] takes it off, telling the log
//! when the option could have done something.

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::build_flags::{EVERY_BUILD, canonical};
use aurcache_db::lists::BuildFlags;
use aurcache_db::packages;
use aurcache_db::prelude::Packages;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, DbErr, EntityTrait, IntoActiveModel, TransactionTrait,
};

/// A stored list of build flags, as a package keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
    /// The offered flags the list chose, canonical.
    pub flags: Vec<String>,
    /// The flags it named that are not kept, as they were stored -- less
    /// [`EVERY_BUILD`], which a worker gives every build anyway, so losing
    /// one changes nothing.
    pub dropped: Vec<String>,
}

/// `stored` reduced to the offered flags it chooses.
///
/// Flag by flag, so a list holding one a build cannot take still keeps the
/// rest; the one it cannot take is dropped with the others.
#[must_use]
pub fn settled(stored: &[impl AsRef<str>]) -> Settled {
    let mut kept: Vec<&str> = Vec::new();
    let mut dropped = Vec::new();
    for flag in stored {
        let flag = flag.as_ref().trim();
        if flag.is_empty() {
            continue;
        }
        match canonical(&[flag]) {
            Ok(offered) if !offered.is_empty() => kept.push(flag),
            _ if EVERY_BUILD.contains(&flag) => {}
            _ => dropped.push(flag.to_string()),
        }
    }
    Settled {
        // Each of `kept` was accepted on its own, so together they are.
        flags: canonical(&kept).unwrap_or_default(),
        dropped,
    }
}

/// Reduce every package's stored build flags to their canonical form, one
/// transaction per package changed, and record which packages lost a flag
/// that could have done something.
///
/// A list already canonical is left alone, so running this again changes
/// nothing.
///
/// # Returns
///
/// How many packages' lists were rewritten.
///
/// # Errors
///
/// The first database error. The packages before it are settled, and the
/// next run picks up the rest.
pub async fn settle(db: &DatabaseConnection, activity: &ActivityLog) -> Result<usize, DbErr> {
    let mut rewritten = 0;
    for pkg in Packages::find().all(db).await? {
        let Settled { flags, dropped } = settled(pkg.build_flags.as_slice());
        if flags.as_slice() == pkg.build_flags.as_slice() {
            continue;
        }
        let name = pkg.name.clone();
        let txn = db.begin().await?;
        let mut row: packages::ActiveModel = pkg.into_active_model();
        row.build_flags = Set(BuildFlags::new(flags));
        row.update(&txn).await?;
        txn.commit().await?;
        rewritten += 1;
        if !dropped.is_empty() {
            activity.emit(Event::BuildFlagsDropped {
                pkg: name.into(),
                dropped,
            });
        }
    }
    Ok(rewritten)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default every package used to be given loses nothing worth saying.
    #[test]
    fn the_old_default_settles_to_nothing_unreported() {
        assert_eq!(
            settled(&["--noconfirm", "--noprogressbar", "--nocolor"]),
            Settled {
                flags: vec![],
                dropped: vec![],
            }
        );
    }

    #[test]
    fn offered_flags_stay_and_the_rest_are_reported() {
        assert_eq!(
            settled(&["--noconfirm", "--cleanbuild", "--skippgpcheck", "--nocheck"]),
            Settled {
                flags: vec!["--nocheck".to_string(), "--skippgpcheck".to_string()],
                dropped: vec!["--cleanbuild".to_string()],
            }
        );
        assert_eq!(
            settled(&["--skipinteg"]).flags,
            ["--skipchecksums", "--skippgpcheck"]
        );
    }

    /// A flag a build cannot take costs the list only that flag.
    #[test]
    fn a_refused_flag_keeps_the_offered_ones() {
        assert_eq!(
            settled(&["--nodeps", "--nobuild", "--nocolor"]),
            Settled {
                flags: vec!["--nodeps".to_string()],
                dropped: vec!["--nobuild".to_string()],
            }
        );
    }

    #[test]
    fn a_canonical_list_is_settled_already() {
        let canonical = ["--nocheck", "--nodeps"];
        assert_eq!(settled(&canonical).flags, canonical);
        assert_eq!(settled(&[] as &[&str]).flags, Vec::<String>::new());
    }

    /// Against a database: each list is rewritten once, and only the package
    /// that lost a flag that could matter is in the log.
    #[tokio::test]
    async fn settling_rewrites_each_list_once() {
        use aurcache_activitylog::activity_utils::{Writer, spawn};
        use aurcache_activitylog::log_store::{LogFilter, LogStore};
        use aurcache_db::testing::{memory_db, package};

        let db = memory_db().await;
        let flags = |stored: &'static str| {
            move |row: &mut packages::ActiveModel| {
                row.build_flags = Set(stored.parse().expect("build flags"));
            }
        };
        package("plain")
            .with(flags("--noconfirm;--noprogressbar;--nocolor"))
            .insert(&db)
            .await;
        package("kept-tree")
            .with(flags("--noconfirm;--cleanbuild;--nocheck"))
            .insert(&db)
            .await;
        package("tidy").with(flags("--nocheck")).insert(&db).await;

        let Writer { log, task } = spawn(db.clone());
        assert_eq!(settle(&db, &log).await.unwrap(), 2);
        assert_eq!(
            settle(&db, &log).await.unwrap(),
            0,
            "a second run changes nothing"
        );
        drop(log);
        task.await.unwrap();

        let mut stored: Vec<String> = Packages::find()
            .all(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|pkg| format!("{} {}", pkg.name, pkg.build_flags))
            .collect();
        stored.sort();
        assert_eq!(stored, ["kept-tree --nocheck", "plain ", "tidy --nocheck"]);

        let page = LogStore::new(db)
            .page(50, 0, &LogFilter::default())
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        let entry = &page.entries[0];
        assert_eq!(entry.kind, "package.build_flags_dropped");
        assert_eq!(entry.data["pkg"], "pkg:kept-tree");
        assert_eq!(entry.data["dropped"], serde_json::json!(["--cleanbuild"]));
    }
}
