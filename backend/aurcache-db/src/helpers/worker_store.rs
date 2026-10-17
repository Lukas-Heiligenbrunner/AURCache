//! Database helpers for remote build workers: enrollment, approval, revocation,
//! and fingerprint lookup used by the mTLS auth guard.

use crate::helpers::time::now_secs;
use crate::lists::WorkerList;
use crate::prelude::{WorkerSettings, Workers};
use crate::{worker_settings, workers};
use aurcache_common::api::worker::ApprovalStatus;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{OnConflict, Query};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder,
    TransactionSession, TransactionTrait,
};
use std::collections::BTreeMap;

/// Everything a worker reports about itself at registration.
///
/// Grouped into a struct because *all* of it is refreshed on every
/// re-registration: the worker's environment is the source of truth, and any of
/// it can have changed since the machine last booted.
pub struct WorkerRegistration<'a> {
    pub name: &'a str,
    pub fingerprint: &'a str,
    pub native_arches: &'a [String],
    pub emulated_arches: &'a [String],
    pub version: &'a str,
    /// Build strategy the worker runs (`chroot`, `docker`).
    pub kind: &'a str,
    /// Exact pkgbase names this worker is provisioned for.
    pub package_affinity: &'a [String],
    /// Scheduling preference; higher wins.
    pub priority: i32,
    /// Maximum concurrent builds the worker will run.
    pub concurrency: i32,
    /// JSON array of the settings the worker declares it accepts, exactly as it
    /// sent them. Replaces the stored one: a worker that stopped declaring a
    /// setting no longer accepts it.
    pub settings_declaration: &'a str,
}

/// Register a worker on first contact, or refresh the existing row when a worker
/// with the same certificate fingerprint re-enrolls (idempotent).
///
/// A freshly registered worker starts in the `pending` state and must be
/// approved before it can claim jobs. Re-registration refreshes every reported
/// field — including `name`, which the operator may have changed — but never
/// touches the approval state: a revoked worker that comes back stays revoked.
pub async fn register_worker<C: ConnectionTrait>(
    db: &C,
    reg: &WorkerRegistration<'_>,
) -> Result<workers::Model, DbErr> {
    let now = now_secs();
    // Insert-if-absent keyed on the unique cert_fingerprint. On conflict we keep
    // the existing row (status/approval preserved) and refresh what the worker
    // reported.
    let insert = Query::insert()
        .into_table(workers::Entity)
        .columns([
            workers::Column::Name,
            workers::Column::Status,
            workers::Column::CertFingerprint,
            workers::Column::NativeArches,
            workers::Column::EmulatedArches,
            workers::Column::LastSeen,
            workers::Column::Version,
            workers::Column::Kind,
            workers::Column::PackageAffinity,
            workers::Column::Priority,
            workers::Column::Concurrency,
            workers::Column::SettingsDeclaration,
        ])
        .values([
            reg.name.into(),
            ApprovalStatus::Pending.into(),
            reg.fingerprint.into(),
            WorkerList::new(reg.native_arches).to_string().into(),
            WorkerList::new(reg.emulated_arches).to_string().into(),
            now.into(),
            reg.version.into(),
            reg.kind.into(),
            WorkerList::new(reg.package_affinity).to_string().into(),
            reg.priority.into(),
            reg.concurrency.into(),
            reg.settings_declaration.into(),
        ])
        .map_err(|e| DbErr::Custom(e.to_string()))?
        .on_conflict(
            OnConflict::column(workers::Column::CertFingerprint)
                .update_columns([
                    workers::Column::Name,
                    workers::Column::NativeArches,
                    workers::Column::EmulatedArches,
                    workers::Column::LastSeen,
                    workers::Column::Version,
                    workers::Column::Kind,
                    workers::Column::PackageAffinity,
                    workers::Column::Priority,
                    workers::Column::Concurrency,
                    workers::Column::SettingsDeclaration,
                ])
                .to_owned(),
        )
        .to_owned();

    db.execute(&insert).await?;

    find_worker_by_fingerprint(db, reg.fingerprint)
        .await?
        .ok_or_else(|| DbErr::Custom("worker vanished after registration".to_string()))
}

/// Look up a worker by id.
pub async fn find_worker<C: ConnectionTrait>(
    db: &C,
    id: i32,
) -> Result<Option<workers::Model>, DbErr> {
    Workers::find_by_id(id).one(db).await
}

/// Look up a worker by its public-key fingerprint (used by the mTLS guard).
pub async fn find_worker_by_fingerprint<C: ConnectionTrait>(
    db: &C,
    fingerprint: &str,
) -> Result<Option<workers::Model>, DbErr> {
    Workers::find()
        .filter(workers::Column::CertFingerprint.eq(fingerprint))
        .one(db)
        .await
}

/// Load a worker as an `ActiveModel` ready to be mutated and updated.
///
/// `None` when the row is gone. Absence is an `Option`, not a `DbErr::Custom`:
/// a caller holding a stale id made a client error, and the endpoints map this
/// to 404 — a database-shaped error for it would surface as a 500 and mask
/// real server faults in monitoring.
async fn load_for_update<C: ConnectionTrait>(
    db: &C,
    id: i32,
) -> Result<Option<workers::ActiveModel>, DbErr> {
    Ok(Workers::find_by_id(id).one(db).await?.map(Into::into))
}

async fn set_status<C: ConnectionTrait>(
    db: &C,
    id: i32,
    status: ApprovalStatus,
) -> Result<Option<workers::Model>, DbErr> {
    let Some(mut active) = load_for_update(db, id).await? else {
        return Ok(None);
    };
    active.status = Set(status);
    Ok(Some(active.update(db).await?))
}

/// Store the CA-signed leaf certificate for a worker (done at registration time,
/// before approval). The certificate chains to the CA but the worker is still
/// refused at the auth guard until its status becomes `approved`.
pub async fn store_signed_cert<C: ConnectionTrait>(
    db: &C,
    id: i32,
    signed_cert: &str,
    not_after: i64,
) -> Result<Option<workers::Model>, DbErr> {
    let Some(mut active) = load_for_update(db, id).await? else {
        return Ok(None);
    };
    active.signed_cert = Set(Some(signed_cert.to_string()));
    active.not_after = Set(Some(not_after));
    Ok(Some(active.update(db).await?))
}

/// Approve a worker so it may claim jobs. The signed certificate is issued at
/// registration time; approval only flips the gating status.
pub async fn approve_worker<C: ConnectionTrait>(
    db: &C,
    id: i32,
) -> Result<Option<workers::Model>, DbErr> {
    set_status(db, id, ApprovalStatus::Approved).await
}

/// Revoke a worker: it is immediately refused at the auth guard, its package
/// affinity reservations are released (they only count *approved* workers), and
/// any build it still holds is abandoned, with a fresh attempt queued for
/// someone else.
///
/// Revoking is also how a machine is retired — worker rows are never deleted, so
/// build history keeps resolving to the machine that produced it. Re-approving
/// is the way back.
pub async fn revoke_worker<C: ConnectionTrait + sea_orm::TransactionTrait>(
    db: &C,
    id: i32,
    max_attempts: i32,
) -> Result<Option<Revoked>, DbErr> {
    let Some(mut active) = load_for_update(db, id).await? else {
        return Ok(None);
    };
    active.status = Set(ApprovalStatus::Revoked);
    // Pausing is how a machine is emptied before it is retired, and it
    // belongs to that retirement: a machine approved again later is expected
    // to build, not to come back still refusing work.
    active.paused = Set(false);
    let worker = active.update(db).await?;
    let abandoned =
        crate::helpers::worker_jobs::abandon_worker_builds(db, id, max_attempts).await?;
    Ok(Some(Revoked { worker, abandoned }))
}

/// Ask a worker to take no new builds (`true`), or to take them again.
///
/// Nothing else changes: its builds keep running, its approval stands, and it
/// keeps heartbeating. The claim query reads this on every claim, so it takes
/// effect at the worker's next one. `None` when there is no such worker.
pub async fn set_paused<C: ConnectionTrait>(
    db: &C,
    id: i32,
    paused: bool,
) -> Result<Option<workers::Model>, DbErr> {
    let Some(mut active) = load_for_update(db, id).await? else {
        return Ok(None);
    };
    active.paused = Set(paused);
    Ok(Some(active.update(db).await?))
}

/// A revoked worker, and the builds taken back from it.
#[derive(Debug)]
pub struct Revoked {
    pub worker: workers::Model,
    /// The builds it held, now failed, each with a fresh attempt in its place
    /// unless its package is gone.
    pub abandoned: Vec<crate::helpers::worker_jobs::Abandoned>,
}

/// List all workers in name order.
///
/// Not by `last_seen`: that is rewritten on every heartbeat, so a "most
/// recently seen" order shuffles the list every few seconds. Name order keeps
/// the fleet in the same place between polls; liveness is a column, not the
/// sort.
pub async fn list_workers<C: ConnectionTrait>(db: &C) -> Result<Vec<workers::Model>, DbErr> {
    Workers::find()
        .order_by_asc(workers::Column::Name)
        .order_by_asc(workers::Column::Id)
        .all(db)
        .await
}

/// Store what a worker reported its settings resolved to.
///
/// Kept whole, as the JSON the worker sent: the server renders this and never
/// reasons about it, so parsing it here would only add a way for a newer
/// worker's report to be lost on the way to the page that shows it.
pub async fn store_effective_config<C: ConnectionTrait>(
    db: &C,
    id: i32,
    effective: &str,
) -> Result<(), DbErr> {
    Workers::update_many()
        .col_expr(workers::Column::EffectiveConfig, Some(effective).into())
        .filter(workers::Column::Id.eq(id))
        .exec(db)
        .await?;
    Ok(())
}

/// The values set for a worker on the server, by key.
///
/// A map ordered by key, because that order is also the snapshot's canonical
/// form: the revision a worker is sent is computed over exactly this.
pub async fn worker_setting_values<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
) -> Result<BTreeMap<String, String>, DbErr> {
    Ok(WorkerSettings::find()
        .filter(worker_settings::Column::WorkerId.eq(worker_id))
        .all(db)
        .await?
        .into_iter()
        .map(|row| (row.key, row.value))
        .collect())
}

/// What a save actually changed, for the one log entry it gets.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SavedSettings {
    /// Keys given a value they did not already have.
    pub set: Vec<String>,
    /// Keys whose stored value was removed.
    pub reset: Vec<String>,
}

impl SavedSettings {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.reset.is_empty()
    }
}

/// Save several of a worker's settings as one change: every row is written in
/// one transaction or none is.
///
/// One transaction because the snapshot a heartbeat delivers is read from
/// committed rows, so a worker can never be sent half of a save -- fewer builds
/// with more memory each, say, arriving as more builds with more memory each.
///
/// `Some` stores the value, `None` removes it. The caller has already checked
/// each value against the worker's declaration; nothing here interprets them.
/// A value equal to the stored one, or the removal of one that is not stored,
/// is not counted as a change.
pub async fn save_worker_settings<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    worker_id: i32,
    changes: &BTreeMap<String, Option<String>>,
) -> Result<SavedSettings, DbErr> {
    let txn = db.begin().await?;
    let before = worker_setting_values(&txn, worker_id).await?;
    let mut saved = SavedSettings::default();
    for (key, value) in changes {
        match value {
            Some(value) if before.get(key) != Some(value) => {
                upsert_worker_setting(&txn, worker_id, key, value).await?;
                saved.set.push(key.clone());
            }
            None if before.contains_key(key) => {
                WorkerSettings::delete_many()
                    .filter(worker_settings::Column::WorkerId.eq(worker_id))
                    .filter(worker_settings::Column::Key.eq(key))
                    .exec(&txn)
                    .await?;
                saved.reset.push(key.clone());
            }
            _ => {}
        }
    }
    txn.commit().await?;
    Ok(saved)
}

/// Write one value, replacing whatever the worker had under that key.
///
/// Public for restore, which writes a restored worker's values inside the
/// transaction that restores the worker itself.
pub async fn upsert_worker_setting<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
    key: &str,
    value: &str,
) -> Result<(), DbErr> {
    WorkerSettings::insert(worker_settings::ActiveModel {
        worker_id: Set(worker_id),
        key: Set(key.to_string()),
        value: Set(value.to_string()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([
            worker_settings::Column::WorkerId,
            worker_settings::Column::Key,
        ])
        .update_column(worker_settings::Column::Value)
        .to_owned(),
    )
    .exec(db)
    .await?;
    Ok(())
}

/// How fresh `last_seen` may be before a touch is skipped.
///
/// Liveness resolves at minute granularity (the timeout is 60 s), so
/// rewriting the row on every log chunk or heartbeat is write
/// amplification for no signal. A touch that carries a *changed* version
/// still writes through immediately — the version is data, not liveness.
const TOUCH_THROTTLE_SECS: i64 = 10;

/// Update a worker's `last_seen` (and optionally its reported version), and
/// `worker` with it.
///
/// Decided against `worker` as the caller read it rather than a fresh read:
/// the auth guard has just loaded the row, and every worker request -- a
/// heartbeat every few seconds, every log chunk -- would otherwise read it
/// twice more.
pub async fn touch_last_seen<C: ConnectionTrait>(
    db: &C,
    worker: &mut workers::Model,
    version: Option<&str>,
) -> Result<(), DbErr> {
    let now = now_secs();
    let version_changed = version.is_some_and(|v| worker.version.as_deref() != Some(v));
    let fresh = worker
        .last_seen
        .is_some_and(|seen| now.saturating_sub(seen) < TOUCH_THROTTLE_SECS);
    if fresh && !version_changed {
        return Ok(());
    }
    let mut update = Workers::update_many()
        .col_expr(workers::Column::LastSeen, Some(now).into())
        .filter(workers::Column::Id.eq(worker.id));
    if let Some(v) = version {
        update = update.col_expr(workers::Column::Version, Some(v).into());
        worker.version = Some(v.to_string());
    }
    update.exec(db).await?;
    worker.last_seen = Some(now);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    /// A list column's entries, for as long as the test runs.
    fn names(entries: &[&str]) -> &'static [String] {
        Box::leak(entries.iter().map(ToString::to_string).collect())
    }

    /// A minimal registration; tests override the fields they care about.
    fn reg<'a>(name: &'a str, fingerprint: &'a str) -> WorkerRegistration<'a> {
        WorkerRegistration {
            name,
            fingerprint,
            native_arches: names(&["x86_64"]),
            emulated_arches: names(&[]),
            version: "0.1.0",
            kind: "chroot",
            package_affinity: names(&[]),
            priority: 0,
            concurrency: 1,
            settings_declaration: "[]",
        }
    }

    /// A worker's declaration is stored verbatim and replaced wholesale when it
    /// registers again: the worker's own code is the source of truth for what
    /// it accepts, and an upgrade that drops a setting must not leave the
    /// server offering it.
    #[tokio::test]
    async fn registration_replaces_the_declaration() {
        let db = setup().await;
        let first = r#"[{"key":"concurrency"}]"#;
        let w = register_worker(
            &db,
            &WorkerRegistration {
                settings_declaration: first,
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(w.settings_declaration, first);
        assert!(w.effective_config.is_none());

        let second = r#"[{"key":"build_timeout"}]"#;
        let w = register_worker(
            &db,
            &WorkerRegistration {
                settings_declaration: second,
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(w.settings_declaration, second);
    }

    /// The effective configuration arrives on the heartbeat, so it is stored
    /// apart from the declaration and survives the next registration.
    #[tokio::test]
    async fn effective_config_is_stored_and_outlives_registration() {
        let db = setup().await;
        let w = register_worker(
            &db,
            &WorkerRegistration {
                settings_declaration: "[]",
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();

        let reported = r#"{"settings":{"concurrency":{"value":"2"}}}"#;
        store_effective_config(&db, w.id, reported).await.unwrap();

        let w = register_worker(
            &db,
            &WorkerRegistration {
                settings_declaration: "[]",
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(w.effective_config.as_deref(), Some(reported));
    }

    /// The kind is stored as reported, and re-registration updates it: a
    /// machine switched from the container builder to the chroot one is the
    /// same worker, and the page must not keep showing the old strategy.
    #[tokio::test]
    async fn registration_records_and_refreshes_the_kind() {
        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        assert_eq!(w.kind.as_deref(), Some("chroot"));

        let w = register_worker(
            &db,
            &WorkerRegistration {
                kind: "docker",
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(w.kind.as_deref(), Some("docker"));
    }

    #[tokio::test]
    async fn register_is_idempotent_by_fingerprint() {
        let db = setup().await;
        let a = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        let b = register_worker(
            &db,
            &WorkerRegistration {
                emulated_arches: names(&["aarch64"]),
                version: "0.2.0",
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(b.status, ApprovalStatus::Pending);
        assert_eq!(b.emulated_arches.to_string(), "aarch64");
        assert_eq!(b.version.as_deref(), Some("0.2.0"));
        assert_eq!(list_workers(&db).await.unwrap().len(), 1);
    }

    /// The worker's environment is the source of truth for its configuration,
    /// so a restart with edited settings must be reflected server-side —
    /// including the name, which is easy to forget in the conflict clause.
    #[tokio::test]
    async fn reregistration_refreshes_all_reported_config() {
        let db = setup().await;
        let w = register_worker(&db, &reg("old-name", "fp-1"))
            .await
            .unwrap();
        approve_worker(&db, w.id).await.unwrap();

        let updated = register_worker(
            &db,
            &WorkerRegistration {
                native_arches: names(&["aarch64"]),
                emulated_arches: names(&["armv7h"]),
                version: "9.9.9",
                package_affinity: names(&["unreal-engine"]),
                priority: 10,
                concurrency: 8,
                ..reg("new-name", "fp-1")
            },
        )
        .await
        .unwrap();

        assert_eq!(updated.id, w.id);
        assert_eq!(updated.name, "new-name");
        assert_eq!(updated.native_arches.to_string(), "aarch64");
        assert_eq!(updated.emulated_arches.to_string(), "armv7h");
        assert_eq!(updated.package_affinity.to_string(), "unreal-engine");
        assert_eq!(updated.priority, 10);
        assert_eq!(updated.concurrency, 8);
        // Refreshing configuration must not disturb the approval decision.
        assert_eq!(updated.status, ApprovalStatus::Approved);
    }

    /// Re-registering is how a returning machine announces itself, so it must
    /// never launder away a revoke.
    #[tokio::test]
    async fn reregistration_does_not_resurrect_a_revoked_worker() {
        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        approve_worker(&db, w.id).await.unwrap();
        revoke_worker(&db, w.id, 3).await.unwrap();

        let back = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        assert_eq!(back.status, ApprovalStatus::Revoked);
        // ...but its liveness is refreshed, so the UI can show it checked in.
        assert!(back.last_seen.is_some());
    }

    #[tokio::test]
    async fn approve_then_revoke_transitions() {
        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        store_signed_cert(&db, w.id, "CERTPEM", 9999).await.unwrap();
        let approved = approve_worker(&db, w.id).await.unwrap().unwrap();
        assert_eq!(approved.status, ApprovalStatus::Approved);
        assert_eq!(approved.signed_cert.as_deref(), Some("CERTPEM"));
        assert_eq!(approved.not_after, Some(9999));

        // Re-registration keeps the approval.
        let re = register_worker(
            &db,
            &WorkerRegistration {
                version: "0.3.0",
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(re.status, ApprovalStatus::Approved);

        let revoked = revoke_worker(&db, w.id, 3).await.unwrap().unwrap();
        assert_eq!(revoked.worker.status, ApprovalStatus::Revoked);
    }

    /// The contract the endpoints rely on: an unknown id is `None`, not a
    /// database-shaped error, so it can map to 404 instead of 500.
    #[tokio::test]
    async fn unknown_worker_status_changes_are_absence_not_error() {
        let db = setup().await;
        assert!(approve_worker(&db, 999).await.unwrap().is_none());
        assert!(revoke_worker(&db, 999, 3).await.unwrap().is_none());
        assert!(
            store_signed_cert(&db, 999, "CERTPEM", 9999)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Revoking must release the worker's grip on work in flight, not leave it
    /// stranded until the lease reaper notices — the worker is refused from this
    /// moment and can never report those builds complete.
    #[tokio::test]
    async fn revoke_abandons_builds_the_worker_still_holds() {
        use aurcache_common::build_state::BuildState;

        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        approve_worker(&db, w.id).await.unwrap();

        db.execute_unprepared(
            "INSERT INTO packages (id, name, status, out_of_date, build_flags, platforms, \
                source_data, directly_requested) \
             VALUES (1, 'p1', 0, 0, '', 'x86_64', '{\"type\":\"aur\",\"name\":\"p1\"}', 1)",
        )
        .await
        .unwrap();
        db.execute_unprepared(&format!(
            "INSERT INTO builds (id, pkg_id, number, status, platform, version, worker_id) \
             VALUES (1, 1, 1, {}, 'x86_64', '1.0', {})",
            BuildState::Active.as_i32(),
            w.id
        ))
        .await
        .unwrap();

        let revoked = revoke_worker(&db, w.id, 3).await.unwrap().unwrap();
        // A revocation spends no retry, so the build always gets one.
        let crate::helpers::worker_jobs::Retry::Queued(retry) = revoked.abandoned[0].retry else {
            panic!("a fresh attempt");
        };

        let row = db
            .query_one_raw(sea_orm::Statement::from_string(
                db.get_database_backend(),
                "SELECT status, worker_id, end_reason FROM builds WHERE id = 1".to_string(),
            ))
            .await
            .unwrap()
            .expect("build exists");
        let status: BuildState = row.try_get("", "status").unwrap();
        let worker_id: Option<i32> = row.try_get("", "worker_id").unwrap();
        let end_reason: Option<aurcache_common::build_state::EndReason> =
            row.try_get("", "end_reason").unwrap();
        assert_eq!(status, BuildState::Failed);
        // Kept: the record of who ran the attempt.
        assert_eq!(worker_id, Some(w.id));
        assert_eq!(
            end_reason,
            Some(aurcache_common::build_state::EndReason::WorkerRevoked)
        );

        let fresh = db
            .query_one_raw(sea_orm::Statement::from_string(
                db.get_database_backend(),
                format!("SELECT status, worker_id FROM builds WHERE id = {retry}"),
            ))
            .await
            .unwrap()
            .expect("the retry exists");
        let status: BuildState = fresh.try_get("", "status").unwrap();
        let worker_id: Option<i32> = fresh.try_get("", "worker_id").unwrap();
        assert_eq!(status, BuildState::Enqueued);
        assert_eq!(worker_id, None);
    }

    #[tokio::test]
    async fn lookup_by_fingerprint() {
        let db = setup().await;
        assert!(
            find_worker_by_fingerprint(&db, "missing")
                .await
                .unwrap()
                .is_none()
        );
        register_worker(&db, &reg("w1", "fp-x")).await.unwrap();
        assert!(
            find_worker_by_fingerprint(&db, "fp-x")
                .await
                .unwrap()
                .is_some()
        );
    }

    /// Liveness touches are throttled: a fresh row is not rewritten, so log
    /// chunks and heartbeats stop paying a write per request — but a
    /// stale row still refreshes and a changed version still writes through.
    #[tokio::test]
    async fn touch_skips_fresh_rows_but_records_staleness_and_versions() {
        async fn last_seen(db: &DatabaseConnection, id: i32) -> Option<i64> {
            Workers::find_by_id(id)
                .one(db)
                .await
                .unwrap()
                .unwrap()
                .last_seen
        }

        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();

        // A stale row refreshes.
        let mut stale: workers::ActiveModel = Workers::find_by_id(w.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
        stale.last_seen = Set(Some(now_secs() - 30));
        stale.update(&db).await.unwrap();
        let mut row = Workers::find_by_id(w.id).one(&db).await.unwrap().unwrap();
        touch_last_seen(&db, &mut row, None).await.unwrap();
        let touched = last_seen(&db, w.id).await.unwrap();
        assert_eq!(row.last_seen, Some(touched), "the caller's row follows");
        assert!(now_secs() - touched < TOUCH_THROTTLE_SECS);

        // A fresh row is left alone.
        touch_last_seen(&db, &mut row, None).await.unwrap();
        assert_eq!(last_seen(&db, w.id).await, Some(touched));

        // A changed version writes through even when fresh.
        touch_last_seen(&db, &mut row, Some("9.9.9")).await.unwrap();
        let bumped = Workers::find_by_id(w.id).one(&db).await.unwrap().unwrap();
        assert_eq!(bumped.version.as_deref(), Some("9.9.9"));
    }

    fn changes(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.map(str::to_string)))
            .collect()
    }

    /// A save writes values, replaces them and removes them in one go, and
    /// reports only what it actually changed -- re-saving a value that is
    /// already stored, or resetting one that is not, is not a change.
    #[tokio::test]
    async fn a_save_sets_replaces_and_removes_values() {
        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();

        let saved = save_worker_settings(
            &db,
            w.id,
            &changes(&[("concurrency", Some("2")), ("build_timeout", Some("6h"))]),
        )
        .await
        .unwrap();
        assert_eq!(saved.set, ["build_timeout", "concurrency"]);
        assert!(saved.reset.is_empty());

        let saved = save_worker_settings(
            &db,
            w.id,
            &changes(&[
                ("concurrency", Some("3")),
                ("build_timeout", None),
                ("never_set", None),
                ("unchanged", None),
            ]),
        )
        .await
        .unwrap();
        assert_eq!(saved.set, ["concurrency"]);
        assert_eq!(saved.reset, ["build_timeout"]);

        let values = worker_setting_values(&db, w.id).await.unwrap();
        assert_eq!(
            values,
            BTreeMap::from([("concurrency".to_string(), "3".to_string())])
        );

        let again = save_worker_settings(&db, w.id, &changes(&[("concurrency", Some("3"))]))
            .await
            .unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    /// Pausing touches nothing but the flag, and a retirement clears it: a
    /// machine approved again later is expected to build.
    #[tokio::test]
    async fn pausing_is_a_flag_that_retirement_clears() {
        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        approve_worker(&db, w.id).await.unwrap();

        let paused = set_paused(&db, w.id, true).await.unwrap().unwrap();
        assert!(paused.paused);
        assert_eq!(paused.status, ApprovalStatus::Approved);

        // Re-registering -- a worker restarting while paused -- keeps it.
        let back = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        assert!(back.paused, "a restart undid the pause");

        revoke_worker(&db, w.id, 3).await.unwrap().unwrap();
        approve_worker(&db, w.id).await.unwrap();
        let again = find_worker(&db, w.id).await.unwrap().unwrap();
        assert!(!again.paused, "a re-approved machine came back paused");

        assert!(set_paused(&db, 9999, true).await.unwrap().is_none());
    }

    /// Values belong to one worker: another's are neither read nor touched.
    #[tokio::test]
    async fn values_are_per_worker() {
        let db = setup().await;
        let a = register_worker(&db, &reg("a", "fp-a")).await.unwrap();
        let b = register_worker(&db, &reg("b", "fp-b")).await.unwrap();
        save_worker_settings(&db, a.id, &changes(&[("concurrency", Some("4"))]))
            .await
            .unwrap();
        save_worker_settings(&db, b.id, &changes(&[("concurrency", None)]))
            .await
            .unwrap();
        assert!(worker_setting_values(&db, b.id).await.unwrap().is_empty());
        assert_eq!(
            worker_setting_values(&db, a.id).await.unwrap()["concurrency"],
            "4"
        );
    }

    /// The values go with their worker: foreign keys are enforced, and a
    /// worker's settings are nobody else's to keep.
    #[tokio::test]
    async fn values_go_with_their_worker() {
        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        save_worker_settings(&db, w.id, &changes(&[("concurrency", Some("2"))]))
            .await
            .unwrap();
        Workers::delete_by_id(w.id).exec(&db).await.unwrap();
        assert_eq!(WorkerSettings::find().all(&db).await.unwrap().len(), 0);
    }
}
