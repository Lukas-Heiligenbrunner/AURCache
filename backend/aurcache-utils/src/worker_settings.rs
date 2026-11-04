//! A worker's configuration: what it declared, what it reports running, and
//! what is set for it here.

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, WorkerReport};
use aurcache_common::api::log::WorkerRef;
use aurcache_common::api::worker::WorkerConfigView;
use aurcache_common::digest::sha256_hex;
use aurcache_common::worker_config::{
    ConfigSnapshot, EffectiveConfig, SettingDecl, SettingStatus, checked_changes,
};
use aurcache_db::helpers::worker_store::{self, SavedSettings};
use aurcache_db::workers;
use sea_orm::{DatabaseConnection, DbErr};
use std::collections::BTreeMap;

/// The revision of a worker's server-set values, as its snapshot carries it.
///
/// A digest of the values themselves, keys in order (a `BTreeMap` serializes
/// that way), so it changes exactly when they do and needs no counter kept in
/// step across restarts. Only ever computed here: a worker echoes it back and
/// never digests anything.
#[must_use]
pub fn config_revision(values: &BTreeMap<String, String>) -> String {
    let canonical = serde_json::to_string(values).unwrap_or_default();
    sha256_hex(canonical)
}

/// The values set for `worker`, when it does not hold revision `received`.
///
/// Never allowed to fail the heartbeat that carries it: a snapshot that cannot
/// be read this time is sent on the next one, while a heartbeat that failed
/// would cost the worker its builds.
pub async fn snapshot_for(
    db: &DatabaseConnection,
    worker: &workers::Model,
    received: Option<&str>,
) -> Option<ConfigSnapshot> {
    let values = worker_store::worker_setting_values(db, worker.id)
        .await
        .map_err(|e| tracing::warn!("reading worker {}'s settings: {e}", worker.id))
        .ok()?;
    let revision = config_revision(&values);
    (received != Some(revision.as_str())).then_some(ConfigSnapshot {
        revision,
        settings: values,
    })
}

/// Store what `worker` reports running, and log what changed since its last
/// report and what it refused.
///
/// A report that cannot be stored is logged rather than returned: it must not
/// cost the worker the heartbeat it rode in on.
pub async fn record_effective(
    db: &DatabaseConnection,
    activity: &ActivityLog,
    worker: &workers::Model,
    effective: &EffectiveConfig,
) {
    let name = || WorkerRef::from(worker.name.as_str());
    let failed = |error: String| Event::WorkerReportFailed {
        worker: name(),
        report: WorkerReport::Configuration,
        error,
    };
    match serde_json::to_string(effective) {
        Ok(json) => {
            if let Err(e) = worker_store::store_effective_config(db, worker.id, &json).await {
                activity.emit(failed(e.to_string()));
            }
        }
        Err(e) => activity.emit(failed(e.to_string())),
    }

    // Against what it last reported. The first report has nothing to differ
    // from, and a restart that comes back the same says nothing.
    if let Some(before) = worker
        .effective_config
        .as_deref()
        .and_then(|raw| serde_json::from_str::<EffectiveConfig>(raw).ok())
    {
        let changed = effective.changed_since(&before);
        if !changed.is_empty() {
            activity.emit(Event::WorkerConfigChanged {
                worker: name(),
                settings: changed,
            });
        }
    }

    // A worker sends this only when it changes, so this is once per worker
    // process rather than once per heartbeat: the log records a machine coming
    // up running something other than what it was configured with.
    let refused: Vec<String> = effective
        .settings
        .iter()
        .filter(|(_, setting)| setting.status == SettingStatus::Rejected)
        .map(|(key, _)| key.clone())
        .collect();
    if !refused.is_empty() {
        activity.emit(Event::WorkerSettingRejected {
            worker: name(),
            settings: refused,
        });
    }
}

/// What [`save`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Save {
    /// Every value is stored; these are the keys that changed.
    Saved(SavedSettings),
    /// Something did not fit the worker's declaration, and nothing was
    /// stored. Every refusal, joined.
    Refused(String),
}

/// Save values for several of `worker`'s settings at once, checked against
/// what it declared: taken whole, or refused whole.
pub async fn save(
    db: &DatabaseConnection,
    worker: &workers::Model,
    changes: BTreeMap<String, Option<String>>,
) -> Result<Save, DbErr> {
    let changes = match checked_changes(&declaration(worker), changes) {
        Ok(changes) => changes,
        Err(why) => return Ok(Save::Refused(why)),
    };
    Ok(Save::Saved(
        worker_store::save_worker_settings(db, worker.id, &changes).await?,
    ))
}

/// A worker's configuration as the page shows it: what it accepts, what it
/// runs, and what is set for it here.
///
/// Both stored halves are parsed here. A blob that no longer parses is
/// reported as absent rather than failing: it means a worker newer than this
/// server in a way the shared vocabulary did not cover, and the rest of the
/// page is still worth showing.
pub async fn view(
    db: &DatabaseConnection,
    worker: &workers::Model,
) -> Result<WorkerConfigView, DbErr> {
    let id = worker.id;
    let values = worker_store::worker_setting_values(db, id).await?;
    Ok(WorkerConfigView {
        worker_id: id,
        revision: config_revision(&values),
        settings: declaration(worker),
        effective: worker
            .effective_config
            .as_deref()
            .and_then(|json| parse_stored(json, id, "configuration report")),
        values,
    })
}

/// How many settings the worker could not use the configured value for.
///
/// A count rather than the report itself: this rides the workers list, which is
/// polled, and what the list needs is only whether there is something to look
/// at. `None` when the worker has reported nothing -- "not known" and "nothing
/// wrong" are different answers, and the page shows them differently.
#[must_use]
pub fn rejected_count(worker: &workers::Model) -> Option<i32> {
    let report: EffectiveConfig = parse_stored(
        worker.effective_config.as_deref()?,
        worker.id,
        "configuration report",
    )?;
    i32::try_from(
        report
            .settings
            .values()
            .filter(|setting| setting.status == SettingStatus::Rejected)
            .count(),
    )
    .ok()
}

/// What `worker` declared it accepts; nothing, when the stored declaration
/// cannot be read.
fn declaration(worker: &workers::Model) -> Vec<SettingDecl> {
    parse_stored(&worker.settings_declaration, worker.id, "declaration").unwrap_or_default()
}

/// Read one of the stored worker-configuration blobs, saying so rather than
/// failing when it cannot be read.
fn parse_stored<T: serde::de::DeserializeOwned>(json: &str, id: i32, what: &str) -> Option<T> {
    serde_json::from_str(json)
        .map_err(|e| {
            tracing::warn!("worker {id}'s stored {what} could not be read: {e}");
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::config_revision;
    use std::collections::BTreeMap;

    /// The revision follows the values alone: equal sets agree whatever order
    /// they were built in, and any change moves it.
    #[test]
    fn the_revision_follows_the_values() {
        let a = BTreeMap::from([
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]);
        let b = BTreeMap::from([
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "1".to_string()),
        ]);
        assert_eq!(config_revision(&a), config_revision(&b));
        let mut c = a.clone();
        c.insert("a".to_string(), "3".to_string());
        assert_ne!(config_revision(&a), config_revision(&c));
        assert_ne!(config_revision(&a), config_revision(&BTreeMap::new()));
    }
}
