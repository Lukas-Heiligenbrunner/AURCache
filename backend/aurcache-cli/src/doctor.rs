//! `aurcli doctor` — why is nothing building?
//!
//! The common failure of a fresh install is not a crash, it is silence: a
//! package sits in the queue and nothing happens. Diagnosing that today means
//! cross-referencing the Workers page against the Builds page and knowing what
//! to look for.
//!
//! Nearly all of the signal already exists — the server computes a
//! [`WaitingReason`] for every build no approved worker can take, and reports
//! per-worker liveness it derives from its own timeout. This module only walks
//! the chain in order and presents it.
//!
//! A failure stops the walk only where it makes what follows meaningless: an
//! unreachable server or a rejected token leaves nothing to ask. A fleet
//! problem does not, because the fleet and the queue together are what name the
//! cause — "no workers enrolled" and "no worker builds x86_64" are one finding
//! reported from both ends.

use crate::cli::OutputFormat;
use anyhow::{Result, bail};
use aurcache_client::{
    ApiReachability, ApprovalStatus, AurCacheClient, Build, WaitingReason, Worker,
};
use aurcache_common::build_state::BuildState;
use serde::Serialize;

/// How many builds to inspect for the queue check.
///
/// The stuck ones are what matter and they sort no particular way, so this is a
/// sample rather than a guarantee. A queue long enough to overflow it has
/// already told the operator something is wrong.
const QUEUE_SAMPLE: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    /// Working, but with something the operator should know about.
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    /// The command or action that fixes it, when there is a single obvious one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Pass,
            detail: detail.into(),
            hint: None,
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Warn,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    const fn is_fatal(&self) -> bool {
        matches!(self.status, Status::Fail)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    /// False if any check failed. A warning does not make the report unhealthy:
    /// it is something to know, not something that stops builds.
    pub ok: bool,
}

impl Report {
    fn new(checks: Vec<Check>) -> Self {
        let ok = !checks.iter().any(Check::is_fatal);
        Self { checks, ok }
    }
}

/// Whether the fleet can build anything at all.
///
/// Ordered by how early the operator gets stuck: nothing enrolled, then
/// enrolled but never approved, then approved but not calling in. Each is a
/// different mistake with a different fix, and lumping them into "no workers
/// available" is what makes this hard to debug in the first place.
#[must_use]
pub fn check_workers(workers: &[Worker]) -> Check {
    if workers.is_empty() {
        return Check::fail(
            "workers",
            "no workers are enrolled",
            "start one with the `docker run` command shown on the Workers page; \
             it will appear here as pending",
        );
    }

    let approved: Vec<&Worker> = workers.iter().filter(|w| w.status.can_build()).collect();
    if approved.is_empty() {
        let pending: Vec<&Worker> = workers
            .iter()
            .filter(|w| w.status == ApprovalStatus::Pending)
            .collect();
        return match pending.first() {
            Some(first) => Check::fail(
                "workers",
                format!("{} worker(s) enrolled, none approved", workers.len()),
                format!("aurcli worker approve {}", first.id),
            ),
            // Everything present is revoked: the rows are kept so build history
            // still resolves, so this reads as an empty fleet rather than a
            // pending one.
            None => Check::fail(
                "workers",
                format!("{} worker(s) enrolled, all revoked", workers.len()),
                "enroll a worker again, or approve one that is still pending",
            ),
        };
    }

    let online = approved.iter().filter(|w| w.online).count();
    if online == 0 {
        return Check::fail(
            "workers",
            format!(
                "{} approved worker(s), none currently connected",
                approved.len()
            ),
            "check the worker container is running and can reach this server",
        );
    }

    Check::pass(
        "workers",
        format!("{} approved, {online} connected", approved.len()),
    )
}

/// Whether anything in the queue is stuck for a reason the server can name.
///
/// A build merely waiting behind a busy worker carries no reason and is not a
/// problem, so it is counted and not flagged.
#[must_use]
pub fn check_queue(builds: &[Build]) -> Check {
    let queued = builds
        .iter()
        .filter(|b| b.status == BuildState::Enqueued)
        .count();

    let stuck: Vec<&Build> = builds
        .iter()
        .filter(|b| b.waiting_reason.is_some())
        .collect();

    let Some(first) = stuck.first() else {
        return Check::pass(
            "queue",
            match queued {
                0 => "nothing queued".to_string(),
                n => format!("{n} queued, all assignable"),
            },
        );
    };

    let detail = stuck
        .iter()
        .filter_map(|b| {
            b.waiting_reason
                .as_ref()
                .map(|reason| format!("{}/{}: {reason}", b.pkg_name, b.number))
        })
        .collect::<Vec<_>>()
        .join("; ");

    // The hint comes from the first reason: several stuck builds almost always
    // share one cause, and three different suggestions would bury it.
    let hint = first
        .waiting_reason
        .as_ref()
        .map_or_else(String::new, hint_for_reason);

    Check::fail(
        "queue",
        format!("{} build(s) cannot be assigned — {detail}", stuck.len()),
        hint,
    )
}

fn hint_for_reason(reason: &WaitingReason) -> String {
    match reason {
        WaitingReason::Arch { arch } => format!(
            "add a worker that builds {arch}, or set WORKER_EMULATED_ARCHES={arch} on an existing one"
        ),
        WaitingReason::Offline => {
            "no capable worker has checked in recently; check the worker container is running"
                .to_string()
        }
        WaitingReason::Affinity { workers } => format!(
            "reserved for {} — bring those back, or revoke them to release the reservation",
            workers.join(", ")
        ),
        WaitingReason::Paused { workers } => format!(
            "intake is stopped on {} — resume one with `aurcli worker resume <id>` once it is ready",
            workers.join(", ")
        ),
    }
}

/// What to do about a URL that reached the site instead of the API.
fn api_url_hint(api_url: &str) -> String {
    let trimmed = api_url.trim_end_matches('/');
    if trimmed.ends_with("/api") {
        format!("`{trimmed}` already names the API, so something else is answering on that address")
    } else {
        format!("point the CLI at the API: `aurcli config set-url {trimmed}/api`")
    }
}

/// Walk the chain, stopping where a failure leaves nothing more to ask: an
/// unreachable server, or a refused token. See the module docs.
pub async fn run_doctor(client: &AurCacheClient, format: OutputFormat) -> Result<()> {
    let api_url = client.base_url();
    let mut checks = Vec::new();

    match client.probe_api().await {
        Ok(ApiReachability::Api) => {
            checks.push(Check::pass("server", format!("reachable at {api_url}")));
        }
        // Answered, but not by the API. Left as a plain reachability pass, this
        // walks on and fails at the token instead, which is the one thing that
        // is not wrong.
        Ok(ApiReachability::NotApi { detail }) => {
            checks.push(Check::fail(
                "server",
                format!("{api_url} answered, but {detail}: that is the web UI, not the API"),
                api_url_hint(api_url),
            ));
            return finish(format, Report::new(checks));
        }
        Err(e) => {
            checks.push(Check::fail(
                "server",
                format!("cannot reach {api_url}: {e:#}"),
                "check the URL with `aurcli config show`, and that the server is running",
            ));
            return finish(format, Report::new(checks));
        }
    }

    match client.user_info().await {
        Ok(user) => checks.push(Check::pass(
            "token",
            user.username.map_or_else(
                || "authenticated (authentication is disabled)".to_string(),
                |name| format!("authenticated as {name}"),
            ),
        )),
        Err(e) => {
            checks.push(Check::fail(
                "token",
                format!("rejected: {e:#}"),
                "set a working token with `aurcli config set-token`",
            ));
            return finish(format, Report::new(checks));
        }
    }

    match client.list_workers().await {
        Ok(workers) => checks.push(check_workers(&workers)),
        Err(e) => checks.push(Check::warn(
            "workers",
            format!("could not be listed: {e:#}"),
            "the fleet could not be inspected, so the queue check below may be misleading",
        )),
    }

    match client
        .list_builds(&aurcache_client::BuildQuery {
            limit: Some(QUEUE_SAMPLE),
            ..Default::default()
        })
        .await
    {
        Ok(builds) => checks.push(check_queue(&builds)),
        Err(e) => checks.push(Check::warn(
            "queue",
            format!("could not be listed: {e:#}"),
            "the queue could not be inspected",
        )),
    }

    finish(format, Report::new(checks))
}

fn finish(format: OutputFormat, report: Report) -> Result<()> {
    match format {
        OutputFormat::Json => crate::output::print_json(&report)?,
        OutputFormat::Text => print_report(&report),
    }

    if report.ok {
        Ok(())
    } else {
        // A non-zero exit is the point for anything scripting this; the detail
        // is already printed above, so the message only has to say how many.
        let failed = report
            .checks
            .iter()
            .filter(|check| check.is_fatal())
            .count();
        bail!("{failed} check(s) failed")
    }
}

fn print_report(report: &Report) {
    let width = report
        .checks
        .iter()
        .map(|check| check.name.len())
        .max()
        .unwrap_or(0);

    for check in &report.checks {
        let marker = match check.status {
            Status::Pass => "✓",
            Status::Warn => "!",
            Status::Fail => "✗",
        };
        println!(
            "{marker} {:<width$}  {}",
            check.name,
            check.detail,
            width = width
        );
        if let Some(hint) = &check.hint {
            // Indented under the line it belongs to, past the marker and the
            // name column, so a hint is never mistaken for another check.
            println!("  {:<width$}  → {hint}", "", width = width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Status, check_queue, check_workers};
    use aurcache_client::{ApprovalStatus, Build, WaitingReason, Worker};
    use aurcache_common::build_state::BuildState;

    fn worker(id: i32, status: ApprovalStatus, online: bool) -> Worker {
        Worker {
            id,
            name: format!("worker-{id}"),
            status,
            cert_fingerprint: "aa".repeat(32),
            native_arches: vec!["x86_64".to_string()],
            emulated_arches: Vec::new(),
            package_affinity: Vec::new(),
            priority: 0,
            last_seen: online.then_some(1),
            version: None,
            kind: None,
            online,
            active_builds: 0,
            successful_builds: 0,
            failed_builds: 0,
            settings_rejected: None,
            paused: false,
        }
    }

    fn build(number: i32, status: BuildState, waiting_reason: Option<WaitingReason>) -> Build {
        Build {
            number,
            pkg_name: "hello".to_string(),
            version: "1.0-1".to_string(),
            status,
            start_time: None,
            end_time: None,
            platform: "x86_64".to_string(),
            size: None,
            peak_memory: None,
            disk_usage: None,
            kept: None,
            worker_name: None,
            log_size: None,
            waiting_reason,
        }
    }

    /// The UI and the API sit on one address, one path apart, so the fix is a
    /// suffix -- and naming it is the whole difference between a diagnosis and
    /// a serde error about column 1.
    #[test]
    fn a_ui_url_is_answered_with_the_api_url() {
        let hint = super::api_url_hint("http://truenas:30099");
        assert!(
            hint.contains("config set-url http://truenas:30099/api"),
            "{hint}"
        );

        let hint = super::api_url_hint("http://truenas:30099/api/");
        assert!(!hint.contains("set-url"), "{hint}");
        assert!(hint.contains("already names the API"), "{hint}");
    }

    #[test]
    fn an_empty_fleet_says_so_rather_than_blaming_the_queue() {
        let check = check_workers(&[]);
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("no workers"), "{:?}", check.detail);
    }

    /// The single most common first-run mistake: the worker started and
    /// enrolled itself, and nobody pressed approve.
    #[test]
    fn a_pending_worker_is_named_in_the_approve_command() {
        let check = check_workers(&[worker(7, ApprovalStatus::Pending, false)]);
        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.hint.as_deref(), Some("aurcli worker approve 7"));
    }

    /// Revoked rows are kept so build history still resolves, so a fleet of
    /// nothing but revoked workers must not read as "waiting for approval".
    #[test]
    fn an_all_revoked_fleet_is_not_reported_as_pending() {
        let check = check_workers(&[worker(1, ApprovalStatus::Revoked, false)]);
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("revoked"), "{:?}", check.detail);
        assert!(
            !check
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("approve 1"),
            "a revoked worker cannot be approved back into service"
        );
    }

    #[test]
    fn approved_but_offline_is_distinct_from_unapproved() {
        let check = check_workers(&[worker(1, ApprovalStatus::Approved, false)]);
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("none currently connected"),
            "{:?}",
            check.detail
        );
    }

    #[test]
    fn one_connected_approved_worker_passes() {
        let check = check_workers(&[
            worker(1, ApprovalStatus::Approved, true),
            worker(2, ApprovalStatus::Pending, false),
        ]);
        assert_eq!(check.status, Status::Pass);
        assert!(check.detail.contains("1 connected"), "{:?}", check.detail);
    }

    /// A build waiting behind a busy worker carries no reason, and is normal.
    #[test]
    fn a_queue_with_no_stated_reason_is_healthy() {
        let check = check_queue(&[build(1, BuildState::Enqueued, None)]);
        assert_eq!(check.status, Status::Pass);
        assert!(check.detail.contains("1 queued"), "{:?}", check.detail);
    }

    #[test]
    fn an_empty_queue_passes() {
        assert_eq!(check_queue(&[]).status, Status::Pass);
    }

    #[test]
    fn a_stuck_build_reports_the_servers_own_reason_and_a_fix() {
        let check = check_queue(&[build(
            3,
            BuildState::Enqueued,
            Some(WaitingReason::Arch {
                arch: "aarch64".to_string(),
            }),
        )]);
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("hello/3"), "{:?}", check.detail);
        assert!(check.detail.contains("aarch64"), "{:?}", check.detail);
        assert!(
            check
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("WORKER_EMULATED_ARCHES"),
            "{:?}",
            check.hint
        );
    }

    /// Affinity reserves a package for named workers; when they are gone the
    /// fix is about those workers, not about architectures.
    #[test]
    fn an_affinity_reservation_names_the_workers_holding_it() {
        let check = check_queue(&[build(
            1,
            BuildState::Enqueued,
            Some(WaitingReason::Affinity {
                workers: vec!["big-iron".to_string()],
            }),
        )]);
        assert_eq!(check.status, Status::Fail);
        assert!(
            check
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("big-iron"),
            "{:?}",
            check.hint
        );
    }

    /// A successful build is not queue trouble, however many of them there are.
    #[test]
    fn finished_builds_do_not_count_towards_the_queue() {
        let check = check_queue(&[build(1, BuildState::Successful, None)]);
        assert_eq!(check.status, Status::Pass);
        assert!(
            check.detail.contains("nothing queued"),
            "{:?}",
            check.detail
        );
    }
}
