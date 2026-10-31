//! `workers`: the fleet, approving and revoking, draining, configuring.

use crate::cli::{OutputFormat, WorkerCommand};
use crate::output::{format_timestamp, print_done_message, print_table, render};
use anyhow::{Context, Result, bail};
use aurcache_client::{
    AurCacheClient, Build, BuildQuery, Worker, WorkerConfigUpdate, WorkerConfigView,
};
use aurcache_common::build_state::BuildState;

pub(crate) async fn run_worker_command(
    client: &AurCacheClient,
    format: OutputFormat,
    command: WorkerCommand,
) -> Result<()> {
    match command {
        WorkerCommand::List => render_workers_list(client, format).await,
        WorkerCommand::Approve { id } => approve_worker_command(client, format, id).await,
        WorkerCommand::Revoke { id } => revoke_worker_command(client, format, id).await,
        WorkerCommand::Pause {
            id,
            wait,
            wait_timeout,
        } => {
            client.pause_worker(id).await?;
            if !wait {
                print_done_message(
                    format,
                    &format!(
                        "intake stopped on worker {id}: new builds go to other workers, the ones \
                         it is running finish"
                    ),
                );
                return Ok(());
            }
            wait_for_drain(client, id, wait_timeout.map(std::time::Duration::from_secs)).await?;
            print_done_message(
                format,
                &format!("worker {id} is paused and running no builds"),
            );
            Ok(())
        }
        WorkerCommand::Resume { id } => {
            client.resume_worker(id).await?;
            print_done_message(format, &format!("intake resumed on worker {id}"));
            Ok(())
        }
        WorkerCommand::Config { id, set, reset } => {
            worker_config_command(client, format, id, &set, &reset).await
        }
    }
}

/// How often a drain is checked. Builds that drain take minutes to hours, so
/// there is nothing to gain from asking the server more often than this.
pub(crate) const DRAIN_POLL: std::time::Duration = std::time::Duration::from_secs(10);

/// Wait until worker `id` holds no running build, reporting on stderr what it
/// still runs whenever that changes.
///
/// Running means `active`: a worker holds a lease on those. Once a build is
/// `publishing` the worker has handed its packages over and let it go.
pub(crate) async fn wait_for_drain(
    client: &AurCacheClient,
    id: i32,
    timeout: Option<std::time::Duration>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let query = BuildQuery {
        worker: Some(id),
        states: vec![BuildState::Active],
        ..BuildQuery::default()
    };
    // Checked again here, by the name the list carries: a server older than
    // these filters ignores them and lists every build, and waiting on other
    // workers' builds would never end.
    let name = client
        .list_workers()
        .await?
        .into_iter()
        .find(|w| w.id == id)
        .map(|w| w.name)
        .with_context(|| format!("no worker {id}"))?;
    let mut last = None;
    loop {
        let mut running = client.list_builds(&query).await?;
        running
            .retain(|b| b.status == BuildState::Active && b.worker_name.as_deref() == Some(&name));
        if running.is_empty() {
            return Ok(());
        }
        let progress = drain_progress(id, &running);
        if last.as_ref() != Some(&progress) {
            eprintln!("{progress}");
            last = Some(progress);
        }
        if let Some(timeout) = timeout
            && started.elapsed() >= timeout
        {
            bail!(
                "worker {id} is still running {} after {}s",
                build_refs(&running),
                timeout.as_secs()
            );
        }
        tokio::time::sleep(DRAIN_POLL).await;
    }
}

/// What a worker is still running, as one line.
pub(crate) fn drain_progress(id: i32, running: &[Build]) -> String {
    let count = running.len();
    let builds = if count == 1 { "build" } else { "builds" };
    format!(
        "waiting for worker {id} to finish {count} {builds}: {}",
        build_refs(running)
    )
}

/// `hello/3, world/1`, sorted so that the line only changes when the builds do.
pub(crate) fn build_refs(builds: &[Build]) -> String {
    let mut refs: Vec<_> = builds
        .iter()
        .map(|b| format!("{}/{}", b.pkg_name, b.number))
        .collect();
    refs.sort();
    refs.join(", ")
}

/// A worker named on the command line: its id, or its name.
pub(crate) async fn resolve_worker(client: &AurCacheClient, worker: &str) -> Result<i32> {
    if let Ok(id) = worker.parse() {
        return Ok(id);
    }
    let workers = client.list_workers().await?;
    workers
        .iter()
        .find(|w| w.name == worker)
        .map(|w| w.id)
        .with_context(|| {
            let names: Vec<_> = workers.iter().map(|w| w.name.as_str()).collect();
            format!("no worker named {worker:?} (workers: {})", names.join(", "))
        })
}

pub(crate) async fn render_workers_list(
    client: &AurCacheClient,
    format: OutputFormat,
) -> Result<()> {
    let workers = client.list_workers().await?;
    render(format, &workers, |workers| print_worker_list(workers))
}

pub(crate) async fn approve_worker_command(
    client: &AurCacheClient,
    format: OutputFormat,
    id: i32,
) -> Result<()> {
    client.approve_worker(id).await?;
    print_done_message(format, &format!("worker {id} approved"));
    Ok(())
}

pub(crate) async fn revoke_worker_command(
    client: &AurCacheClient,
    format: OutputFormat,
    id: i32,
) -> Result<()> {
    client.revoke_worker(id).await?;
    print_done_message(format, &format!("worker {id} revoked"));
    Ok(())
}

pub(crate) async fn worker_config_command(
    client: &AurCacheClient,
    format: OutputFormat,
    id: i32,
    set: &[String],
    reset: &[String],
) -> Result<()> {
    let view = if set.is_empty() && reset.is_empty() {
        client.worker_config(id).await?
    } else {
        let update = worker_config_update(set, reset)?;
        let view = client.update_worker_config(id, &update).await?;
        print_done_message(
            format,
            &format!("saved; worker {id} picks it up on its next heartbeat"),
        );
        view
    };
    render(format, &view, print_worker_config)
}

/// The `--set` and `--reset` arguments as one save.
pub(crate) fn worker_config_update(set: &[String], reset: &[String]) -> Result<WorkerConfigUpdate> {
    let mut settings = std::collections::BTreeMap::new();
    for pair in set {
        let (key, value) = pair
            .split_once('=')
            .with_context(|| format!("--set takes key=value, not {pair:?}"))?;
        settings.insert(key.trim().to_string(), Some(value.to_string()));
    }
    for key in reset {
        anyhow::ensure!(
            !settings.contains_key(key.trim()),
            "{key} is both set and reset"
        );
        settings.insert(key.trim().to_string(), None);
    }
    Ok(WorkerConfigUpdate { settings })
}

pub(crate) fn print_worker_config(view: &WorkerConfigView) {
    use aurcache_common::worker_config::{EffectiveSource, SettingStatus};
    let declared = &view.settings;
    if declared.is_empty() {
        println!("This worker declares no settings.");
        return;
    }
    let effective = view.effective.as_ref();
    let rows = declared
        .iter()
        .map(|decl| {
            let running = effective.and_then(|e| e.settings.get(&decl.key));
            let source = running.map_or("-", |r| match r.source {
                EffectiveSource::Env => "pinned",
                EffectiveSource::Server => "server",
                EffectiveSource::EnvDefault => "env default",
                EffectiveSource::Default => "default",
            });
            let status = running.map_or("not reported", |r| match r.status {
                SettingStatus::Applied => "",
                SettingStatus::Overridden => "overridden",
                SettingStatus::Unsupported => "unsupported",
                SettingStatus::Rejected => "refused",
            });
            vec![
                decl.key.clone(),
                running
                    .and_then(|r| r.value.clone())
                    .unwrap_or_else(|| "-".to_string()),
                source.to_string(),
                view.values
                    .get(&decl.key)
                    .cloned()
                    .unwrap_or_else(|| "-".to_string()),
                status.to_string(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["key", "running", "from", "set here", "status"], &rows);

    // Values for keys the worker no longer declares are kept, not dropped,
    // and are worth a line of their own: nothing above would show them.
    for (key, value) in &view.values {
        if !declared.iter().any(|decl| &decl.key == key) {
            println!("{key}={value} is set here but no longer offered by this worker");
        }
    }
    let pending =
        Some(view.revision.as_str()) != effective.and_then(|e| e.received_revision.as_deref());
    if pending && !view.values.is_empty() {
        println!("The worker has not picked up the latest save yet.");
    }
}

/// A list as comma-separated text, or a dash when there is nothing in it.
pub(crate) fn or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(",")
    }
}

pub(crate) fn print_worker_list(workers: &[Worker]) {
    let rows = workers
        .iter()
        .map(|w| {
            let fp = if w.cert_fingerprint.len() > 16 {
                format!("{}…", &w.cert_fingerprint[..16])
            } else {
                w.cert_fingerprint.clone()
            };
            vec![
                w.id.to_string(),
                w.name.clone(),
                // `as_str`, not `Debug`-lowercased: the display spelling is a
                // contract, not a reflection of the variant name. Paused is
                // a state of an approved worker, so it reads as one.
                if w.paused {
                    format!("{} (intake stopped)", w.status.as_str())
                } else {
                    w.status.as_str().to_string()
                },
                // Which build strategy: `chroot`, `docker`, or whatever a
                // future executor calls itself. A dash for a worker that
                // enrolled before workers reported one.
                w.kind.clone().unwrap_or_else(|| "-".to_string()),
                or_dash(&w.arches),
                or_dash(&w.package_affinity),
                if w.priority == 0 {
                    // Zero is the default and means "no preference"; printing
                    // it would suggest the fleet had been tuned when it has not.
                    "-".to_string()
                } else {
                    w.priority.to_string()
                },
                w.version.clone().unwrap_or_else(|| "-".to_string()),
                format_timestamp(w.last_seen),
                fp,
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &[
            "id",
            "name",
            "status",
            "kind",
            "arches",
            "affinity",
            "priority",
            "version",
            "last_seen",
            "fingerprint",
        ],
        &rows,
    );
}

#[cfg(test)]
mod worker_config_tests {
    use super::worker_config_update;

    /// Sets and resets travel as one save; a value may itself contain `=`.
    #[test]
    fn sets_and_resets_become_one_save() {
        let update = worker_config_update(
            &[
                "concurrency=3".to_string(),
                "keyserver=hkps://k?a=b".to_string(),
            ],
            &["build_timeout".to_string()],
        )
        .unwrap();
        assert_eq!(update.settings["concurrency"].as_deref(), Some("3"));
        assert_eq!(
            update.settings["keyserver"].as_deref(),
            Some("hkps://k?a=b")
        );
        assert_eq!(update.settings["build_timeout"], None);
    }

    #[test]
    fn a_set_without_a_value_is_refused() {
        assert!(worker_config_update(&["concurrency".to_string()], &[]).is_err());
    }

    /// Asking for both at once is a mistake to point out, not a race to settle.
    #[test]
    fn setting_and_resetting_one_key_is_refused() {
        assert!(
            worker_config_update(&["concurrency=3".to_string()], &["concurrency".to_string()])
                .is_err()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builds::tests::{ACTIVE, build};

    /// The progress line only changes when the set of builds does, whatever
    /// order the server lists them in.
    #[test]
    fn drain_progress_names_what_is_still_running() {
        let one = [build("hello", 3, ACTIVE)];
        assert_eq!(
            drain_progress(4, &one),
            "waiting for worker 4 to finish 1 build: hello/3"
        );
        let two = [build("world", 1, ACTIVE), build("hello", 3, ACTIVE)];
        let swapped = [build("hello", 3, ACTIVE), build("world", 1, ACTIVE)];
        assert_eq!(drain_progress(4, &two), drain_progress(4, &swapped));
        assert!(drain_progress(4, &two).contains("2 builds: hello/3, world/1"));
    }
}
