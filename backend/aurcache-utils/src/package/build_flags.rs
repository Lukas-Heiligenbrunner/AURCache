//! Holding every package's stored build flags to the ones a package chooses.
//!
//! A package stores only offered flags, in their canonical form (see
//! [`aurcache_common::build_flags`]). [`settle`] takes any other option off a
//! stored list, telling the log when the option could have done something.

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::build_flags::{EVERY_BUILD, Kind, classify, kept};
use aurcache_db::lists::BuildFlags;
use aurcache_db::packages;
use aurcache_db::prelude::Packages;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, DatabaseConnection, DbErr, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
};

/// What a stored list of build flags chooses, and what it loses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choices {
    /// The offered flags the list chose, canonical.
    pub flags: Vec<String>,
    /// The other options it named, each once and by its long form where
    /// makepkg has one -- less [`EVERY_BUILD`], which a worker gives every
    /// build anyway, so losing one changes nothing.
    pub dropped: Vec<String>,
}

/// The offered flags `stored` chooses, and the options it loses.
///
/// Option by option, so a list naming one a build cannot take still keeps the
/// rest, even within one bundle: `-do` keeps `--nodeps` and loses `--nobuild`.
#[must_use]
pub fn choices(stored: &[impl AsRef<str>]) -> Choices {
    let options = classify(stored);
    let mut dropped: Vec<String> = Vec::new();
    for option in &options {
        let reported = match option.kind {
            Kind::Kept(_) => false,
            Kind::Dropped => !EVERY_BUILD.contains(&option.option.as_str()),
            Kind::Refused(_) => true,
        };
        if reported && !dropped.contains(&option.option) {
            dropped.push(option.option.clone());
        }
    }
    Choices {
        flags: kept(&options),
        dropped,
    }
}

/// The columns [`settle`] reads: only these, so a row it does not need to
/// understand cannot stop it.
#[derive(FromQueryResult)]
struct StoredFlags {
    id: i32,
    name: String,
    build_flags: BuildFlags,
}

/// Reduce every package's stored build flags to their canonical form, one
/// statement per package changed, and record which packages lost a flag that
/// could have done something.
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
    let stored = Packages::find()
        .select_only()
        .column(packages::Column::Id)
        .column(packages::Column::Name)
        .column(packages::Column::BuildFlags)
        .into_model::<StoredFlags>()
        .all(db)
        .await?;
    let mut rewritten = 0;
    for pkg in stored {
        let Choices { flags, dropped } = choices(pkg.build_flags.as_slice());
        if flags.as_slice() == pkg.build_flags.as_slice() {
            continue;
        }
        Packages::update_many()
            .col_expr(
                packages::Column::BuildFlags,
                Expr::value(BuildFlags::new(flags)),
            )
            .filter(packages::Column::Id.eq(pkg.id))
            .exec(db)
            .await?;
        rewritten += 1;
        if !dropped.is_empty() {
            activity.emit(Event::BuildFlagsDropped {
                pkg: pkg.name.into(),
                dropped,
            });
        }
    }
    Ok(rewritten)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flags a worker gives every build are lost without a word.
    #[test]
    fn every_build_flags_settle_to_nothing_unreported() {
        assert_eq!(
            choices(&["--noconfirm", "--noprogressbar", "--nocolor", "-m"]),
            Choices {
                flags: vec![],
                dropped: vec![],
            }
        );
    }

    #[test]
    fn offered_flags_stay_and_the_rest_are_reported() {
        assert_eq!(
            choices(&["--noconfirm", "--cleanbuild", "--skippgpcheck", "--nocheck"]),
            Choices {
                flags: vec!["--nocheck".to_string(), "--skippgpcheck".to_string()],
                dropped: vec!["--cleanbuild".to_string()],
            }
        );
        assert_eq!(
            choices(&["--skipinteg"]).flags,
            ["--skipchecksums", "--skippgpcheck"]
        );
    }

    /// A flag a build cannot take costs the list only that flag.
    #[test]
    fn a_refused_flag_keeps_the_offered_ones() {
        assert_eq!(
            choices(&["--nodeps", "--nobuild", "--nocolor"]),
            Choices {
                flags: vec!["--nodeps".to_string()],
                dropped: vec!["--nobuild".to_string()],
            }
        );
    }

    /// A bundle is settled letter by letter, and what it loses is reported by
    /// its long form.
    #[test]
    fn a_bundle_settles_letter_by_letter() {
        let choices_of = |stored: &str| choices(&[stored]);
        assert_eq!(
            choices_of("-cdo"),
            Choices {
                flags: vec!["--nodeps".to_string()],
                dropped: vec!["--clean".to_string(), "--nobuild".to_string()],
            }
        );
        assert_eq!(
            choices_of("-cd"),
            Choices {
                flags: vec!["--nodeps".to_string()],
                dropped: vec!["--clean".to_string()],
            }
        );
        assert_eq!(
            choices_of("-m"),
            Choices {
                flags: vec![],
                dropped: vec![],
            }
        );
    }

    #[test]
    fn a_canonical_list_is_settled_already() {
        let canonical = ["--nocheck", "--nodeps"];
        assert_eq!(choices(&canonical).flags, canonical);
        assert_eq!(choices(&[] as &[&str]).flags, Vec::<String>::new());
    }

    /// Against a database: each list is rewritten once, and only the package
    /// that lost a flag that could matter is in the log.
    #[tokio::test]
    async fn settling_rewrites_each_list_once() {
        use aurcache_activitylog::activity_utils::{Writer, spawn};
        use aurcache_activitylog::log_store::{LogFilter, LogStore};
        use aurcache_db::testing::{memory_db, package};
        use sea_orm::ActiveValue::Set;

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
