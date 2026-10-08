//! `builds`: listing, showing and retrying builds, and following them as
//! they run.

use crate::cli::{
    BuildOutputArgs, BuildRef, BuildsCommand, ListBuildsArgs, OutputFormat, WaitOpts, WatchArgs,
};
use crate::output::{
    Progress, format_timestamp, print_done_message, print_json, print_table, render,
};
use crate::workers::resolve_worker;
use anyhow::{Result, bail};
use aurcache_client::{AurCacheClient, Build, BuildQuery};
use aurcache_common::api::build_log::{Alignment, align};
use aurcache_common::build_state::BuildState;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::io::Write;

pub(crate) async fn run_builds_command(
    client: &AurCacheClient,
    format: OutputFormat,
    command: BuildsCommand,
) -> Result<()> {
    match command {
        BuildsCommand::List(args) => render_builds_list(client, format, args).await,
        BuildsCommand::Get { build } => render_build(client, format, build).await,
        BuildsCommand::Output(args) => render_build_output(client, format, args).await,
        BuildsCommand::Retry { build, wait } => {
            retry_build_command(client, format, build, wait).await
        }
        BuildsCommand::Cancel { build } => cancel_build_command(client, format, build).await,
        BuildsCommand::Delete { build } => delete_build_command(client, format, build).await,
        BuildsCommand::Watch(args) => watch_builds_command(client, format, args).await,
    }
}

pub(crate) async fn render_builds_list(
    client: &AurCacheClient,
    format: OutputFormat,
    args: ListBuildsArgs,
) -> Result<()> {
    let worker = match args.worker.as_deref() {
        Some(worker) => Some(resolve_worker(client, worker).await?),
        None => None,
    };
    let builds = client
        .list_builds(&BuildQuery {
            pkgbase: args.pkgbase,
            worker,
            states: args.status,
            limit: args.limit,
            page: args.page,
        })
        .await?;
    render(format, &builds, |builds| print_build_list(builds))
}

pub(crate) async fn render_build(
    client: &AurCacheClient,
    format: OutputFormat,
    build: BuildRef,
) -> Result<()> {
    let build = client.get_build(&build.pkgbase, build.number).await?;
    render(format, &build, print_build)
}

pub(crate) async fn render_build_output(
    client: &AurCacheClient,
    format: OutputFormat,
    args: BuildOutputArgs,
) -> Result<()> {
    // The log is walked in bounded pages with byte offsets, so one fetch never
    // costs in proportion to the whole log. Each page is re-aligned to UTF-8
    // and a character split across two pages is re-read whole on the next one;
    // the offset arithmetic is the same `align` the frontend uses, so both ends
    // of the API walk identical bytes.
    let mut offset = args.offset.unwrap_or(0);
    // `--limit` caps the total bytes *printed*; without it the whole log is
    // printed, one bounded page at a time.
    let mut remaining = args.limit;
    // JSON accumulates one output string, matching the shape of the plain view.
    let mut accumulated = String::new();

    loop {
        let page = client
            .build_output_page(&args.build.pkgbase, args.build.number, Some(offset), None)
            .await?;
        if page.is_empty() {
            break;
        }
        let Alignment {
            front_skip,
            back_drop,
        } = align(&page);
        // When the whole page is a split character, `back_drop` covers it and
        // the offset cannot advance. That is EOF mid-character — stop, rather
        // than re-read the same tail forever (the empty-page rule alone would
        // never fire, because the re-read is non-empty).
        let advanced = page.len().saturating_sub(back_drop) as u64;
        if advanced == 0 {
            break;
        }

        let aligned = &page[front_skip..page.len() - back_drop];
        // Trim to the byte cap before decoding, honoring the "--limit is bytes"
        // promise. A cut character becomes a replacement char in the last page.
        let (slice, capped) = match remaining {
            Some(r) if r < aligned.len() as u64 => (&aligned[..r as usize], true),
            _ => (aligned, false),
        };
        let decoded = String::from_utf8_lossy(slice);
        match format {
            OutputFormat::Text => {
                print!("{decoded}");
                std::io::stdout().flush()?;
            }
            OutputFormat::Json => accumulated.push_str(&decoded),
        }

        if let Some(r) = remaining.as_mut() {
            *r -= slice.len() as u64;
        }
        offset += advanced;
        if capped || remaining == Some(0) {
            break;
        }
    }

    if format == OutputFormat::Json {
        return print_json(&json!({ "output": accumulated }));
    }
    Ok(())
}

pub(crate) async fn retry_build_command(
    client: &AurCacheClient,
    format: OutputFormat,
    build: BuildRef,
    wait: WaitOpts,
) -> Result<()> {
    let scope = if wait.wait {
        Some(snapshot_builds(client, None).await?)
    } else {
        None
    };
    let number = client.retry_build(&build.pkgbase, build.number).await?;
    match format {
        OutputFormat::Json => print_json(&number)?,
        OutputFormat::Text => println!("enqueued build: {}/{number}", build.pkgbase),
    }
    let Some(scope) = scope else {
        return Ok(());
    };
    let scope = scope
        .with_explicit([BuildRef {
            pkgbase: build.pkgbase.clone(),
            number,
        }])
        .adopting(&build.pkgbase);
    follow_builds(client, Progress(format), &wait.as_watch_args(), scope).await
}

pub(crate) async fn cancel_build_command(
    client: &AurCacheClient,
    format: OutputFormat,
    build: BuildRef,
) -> Result<()> {
    client.cancel_build(&build.pkgbase, build.number).await?;
    print_done_message(format, "build cancelled");
    Ok(())
}

pub(crate) async fn delete_build_command(
    client: &AurCacheClient,
    format: OutputFormat,
    build: BuildRef,
) -> Result<()> {
    client.delete_build(&build.pkgbase, build.number).await?;
    print_done_message(format, "build deleted");
    Ok(())
}

/// How often `watch` re-lists builds.
pub(crate) const WATCH_POLL_INTERVAL_SECS: u64 = 5;

pub(crate) fn print_build_list(builds: &[Build]) {
    let rows = builds
        .iter()
        .map(|build| {
            vec![
                // The build's public name, the same form the argument parser
                // accepts, so a row can be copied straight into another
                // command. It already names the package, so there is no
                // separate column for that.
                format!("{}/{}", build.pkg_name, build.number),
                build.platform.clone(),
                build.status.label().to_string(),
                build.version.clone(),
                format_timestamp(build.start_time),
                format_timestamp(build.end_time),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &[
            "build",
            "platform",
            "status",
            "version",
            "start_time",
            "end_time",
        ],
        &rows,
    );
}

pub(crate) fn print_build(build: &Build) {
    println!("build: {}/{}", build.pkg_name, build.number);
    println!("platform: {}", build.platform);
    println!("status: {}", build.status.label());
    println!("version: {}", build.version);
    println!("start_time: {}", format_timestamp(build.start_time));
    println!("end_time: {}", format_timestamp(build.end_time));
}

/// A build's public identity: the row id is not part of the API.
pub(crate) fn build_ref(build: &Build) -> BuildRef {
    BuildRef {
        pkgbase: build.pkg_name.clone(),
        number: build.number,
    }
}

/// Which builds one invocation is answerable for.
///
/// Every listing mixes the work this invocation is about with whatever else the
/// server has ever done, and the difference cannot be recovered from the rows:
/// a build that failed last week and one that failed a second ago look
/// identical. So the caller states it, once, before any polling — and what it
/// can state depends on what it knows:
///
/// * `builds watch` learns it from its own first listing: anything already
///   finished by then is somebody else's history, because a build it never saw
///   run is not one it can report on.
/// * a `--wait` trigger takes its listing *before* sending the request, so
///   everything that appears afterwards is its own doing — including a build
///   that failed before the response came back, which is precisely the case a
///   watcher started afterwards can never see.
#[derive(Debug, Default, Clone)]
pub(crate) struct WatchScope {
    /// Builds that were already there and are not ours.
    ignore: HashSet<BuildRef>,
    /// Ours by name, whatever state they are in when first seen. This is what
    /// makes a build the server told us it queued impossible to lose.
    explicit: HashSet<BuildRef>,
    /// Packages whose unfinished builds we adopt even though they predate us:
    /// a trigger that reused an existing build row rather than creating one
    /// still has to wait for it.
    adopt: HashSet<String>,
}

impl WatchScope {
    /// For `builds watch`: only the already-finished builds are history.
    /// Anything still in flight is happening now and is worth following.
    pub(crate) fn from_history(builds: &[Build]) -> Self {
        Self {
            ignore: builds
                .iter()
                .filter(|b| !b.status.is_in_progress())
                .map(build_ref)
                .collect(),
            ..Self::default()
        }
    }

    /// For `--wait`: everything that existed before the trigger is somebody
    /// else's, running or not, so the verdict covers this trigger's work alone.
    pub(crate) fn from_snapshot(builds: &[Build]) -> Self {
        Self {
            ignore: builds.iter().map(build_ref).collect(),
            ..Self::default()
        }
    }

    pub(crate) fn with_explicit(mut self, keys: impl IntoIterator<Item = BuildRef>) -> Self {
        self.explicit.extend(keys);
        self
    }

    pub(crate) fn adopting(mut self, pkgbase: &str) -> Self {
        self.adopt.insert(pkgbase.to_string());
        self
    }

    pub(crate) fn includes(&self, build: &Build) -> bool {
        let key = build_ref(build);
        if self.explicit.contains(&key) {
            return true;
        }
        if !self.ignore.contains(&key) {
            return true;
        }
        self.adopt.contains(&build.pkg_name) && build.status.is_in_progress()
    }
}

/// Follow builds until they settle, printing transitions and detecting stalls.
///
/// Written for humans watching a queue and for scripts driving one: it reports
/// what changed rather than repeating the current state, and it fails fast when
/// the queue cannot progress instead of waiting out the timeout.
pub(crate) async fn watch_builds_command(
    client: &AurCacheClient,
    format: OutputFormat,
    args: WatchArgs,
) -> Result<()> {
    let progress = Progress(format);
    let builds = list_watched_builds(client, args.package.as_deref()).await?;
    let scope = WatchScope::from_history(&builds).with_explicit(args.builds.iter().cloned());

    // Said once, rather than as a burst of transition lines for builds that
    // transitioned before anyone was watching. Failures among them are named
    // because they are the reason the exit code may not be what a reader of
    // the list expects.
    let history: Vec<&Build> = builds.iter().filter(|b| !scope.includes(b)).collect();
    if !history.is_empty() {
        let failed = history
            .iter()
            .filter(|b| b.status == BuildState::Failed)
            .count();
        let failed = if failed == 0 {
            String::new()
        } else {
            format!(", {failed} of them failed")
        };
        progress.line(&format!(
            "not following {} build(s) that finished before this{failed}",
            history.len()
        ));
    }

    follow_builds(client, progress, &args, scope).await
}

/// Poll until every build in scope has settled.
///
/// Stall detection deliberately looks at the *whole* queue rather than the
/// scope: a build waiting on a dependency is not stalled while that dependency
/// — a different package, outside a `--package` filter — is building. The
/// question is whether the queue can advance, not whether our part of it is
/// moving.
pub(crate) async fn follow_builds(
    client: &AurCacheClient,
    progress: Progress,
    args: &WatchArgs,
    scope: WatchScope,
) -> Result<()> {
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let mut last_change = Instant::now();
    let mut last_beat = Instant::now();
    let mut seen: HashMap<BuildRef, BuildState> = HashMap::new();

    loop {
        let builds = list_watched_builds(client, args.package.as_deref()).await?;
        let (watched, rest): (Vec<&Build>, Vec<&Build>) =
            builds.iter().partition(|b| scope.includes(b));

        let mut changed = false;
        for build in &watched {
            let key = build_ref(build);
            if seen.get(&key) != Some(&build.status) {
                let elapsed = start.elapsed().as_secs();
                let reason = build
                    .waiting_reason
                    .as_ref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default();
                progress.line(&format!(
                    "[{elapsed:>4}s] {}/{}: {}{reason}",
                    build.pkg_name,
                    build.number,
                    build.status.label(),
                ));
                seen.insert(key, build.status);
                changed = true;
            }
        }
        if changed {
            last_change = Instant::now();
        }

        // Settled when every build in scope has reached a terminal state. An
        // empty scope is settled too: there was nothing to follow, which is an
        // answer rather than a reason to wait — a `--wait` trigger that queued
        // nothing says so at once instead of sitting out the stall timeout.
        if watched.iter().all(|b| !b.status.is_in_progress()) {
            let failed: Vec<String> = watched
                .iter()
                .filter(|b| b.status == BuildState::Failed)
                .map(|b| format!("{}/{}", b.pkg_name, b.number))
                .collect();
            if !failed.is_empty() {
                bail!("build failed: {}", failed.join(", "));
            }
            if watched.is_empty() {
                progress.line("nothing to follow");
            } else {
                progress.line(&format!(
                    "{} build(s) succeeded in {}s",
                    watched.len(),
                    start.elapsed().as_secs()
                ));
            }
            return Ok(());
        }

        let anything_running = builds.iter().any(|b| b.status == BuildState::Active);

        if !anything_running && last_change.elapsed() >= Duration::from_secs(args.stall_after) {
            for build in watched.iter().filter(|b| b.status.is_in_progress()) {
                let reason = build
                    .waiting_reason
                    .as_ref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default();
                eprintln!(
                    "  {}/{}: {}{reason}",
                    build.pkg_name,
                    build.number,
                    build.status.label()
                );
            }
            bail!(
                "no progress for {}s and nothing is building; the queue cannot advance",
                last_change.elapsed().as_secs()
            );
        }

        if last_beat.elapsed() >= Duration::from_secs(args.heartbeat) {
            let active = watched
                .iter()
                .filter(|b| b.status == BuildState::Active)
                .count();
            progress.line(&format!(
                "[{:>4}s] {} building, {} of {} finished{}",
                start.elapsed().as_secs(),
                active,
                watched
                    .iter()
                    .filter(|b| !b.status.is_in_progress())
                    .count(),
                watched.len(),
                // Only work still in flight: that the server has a hundred
                // finished builds on record is not news, but that something
                // else is running explains why ours is waiting.
                match rest.iter().filter(|b| b.status.is_in_progress()).count() {
                    0 => String::new(),
                    other => format!(" ({other} other build(s) in flight, not followed)"),
                }
            ));
            last_beat = Instant::now();
        }

        if start.elapsed() >= Duration::from_secs(args.timeout) {
            bail!("timed out after {}s", args.timeout);
        }
        tokio::time::sleep(Duration::from_secs(WATCH_POLL_INTERVAL_SECS)).await;
    }
}

/// One page of builds, optionally narrowed to a package.
///
/// The page bounds how far back the scope can see, which is why the scope is
/// taken as a set of identities rather than by counting: an old build dropping
/// off the end of the page must not change the verdict.
pub(crate) async fn list_watched_builds(
    client: &AurCacheClient,
    package: Option<&str>,
) -> Result<Vec<Build>> {
    Ok(client
        .list_builds(&aurcache_client::BuildQuery {
            limit: Some(100),
            ..Default::default()
        })
        .await?
        .into_iter()
        .filter(|b| package.is_none_or(|name| b.pkg_name == name))
        .collect())
}

/// The listing a `--wait` trigger takes *before* it fires, which is the whole
/// reason `--wait` can report on a build that finished before the trigger
/// returned.
pub(crate) async fn snapshot_builds(
    client: &AurCacheClient,
    package: Option<&str>,
) -> Result<WatchScope> {
    Ok(WatchScope::from_snapshot(
        &list_watched_builds(client, package).await?,
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A build row as the list endpoint returns one; only identity and status
    /// matter to a scope.
    pub(crate) fn build(pkg: &str, number: i32, status: BuildState) -> Build {
        Build {
            number,
            pkg_name: pkg.to_string(),
            version: "1-1".to_string(),
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
            waiting_reason: None,
        }
    }

    pub(crate) const ACTIVE: BuildState = BuildState::Active;

    pub(crate) const FAILED: BuildState = BuildState::Failed;

    pub(crate) const SUCCESSFUL: BuildState = BuildState::Successful;

    /// `builds watch` reports on what it watched. A build that was already over
    /// before it looked is history it cannot speak for -- and on a long-lived
    /// instance every listing is mostly history, which is what used to make the
    /// exit code a statement about last week.
    #[test]
    fn watch_follows_what_is_in_flight_and_not_old_builds() {
        let history = [
            build("fonts", 5, FAILED),
            build("fonts", 6, FAILED),
            build("fonts", 7, SUCCESSFUL),
        ];
        let scope = WatchScope::from_history(&history);

        assert!(!history.iter().any(|b| scope.includes(b)));
        assert!(
            scope.includes(&build("fonts", 8, ACTIVE)),
            "a build that appears later is this watch's business"
        );
        assert!(
            scope.includes(&build("fonts", 9, FAILED)),
            "including one that has already failed by the time we see it: it \
             failed while we were watching"
        );
    }

    /// Anything still running when the watch starts is in scope without being
    /// named: it is happening now.
    #[test]
    fn watch_follows_a_build_that_was_already_running() {
        let scope = WatchScope::from_history(&[build("fonts", 8, ACTIVE)]);
        assert!(scope.includes(&build("fonts", 8, ACTIVE)));
        assert!(scope.includes(&build("fonts", 8, FAILED)));
    }

    /// The race `--wait` exists for: the trigger queues a build that fails
    /// before the request even returns. A watcher started afterwards sees an
    /// old failed build and cannot tell; a snapshot taken beforehand can.
    #[test]
    fn wait_catches_a_build_that_failed_before_the_trigger_returned() {
        let before = [build("fonts", 7, SUCCESSFUL), build("other", 2, ACTIVE)];
        let scope = WatchScope::from_snapshot(&before);

        assert!(
            scope.includes(&build("fonts", 8, FAILED)),
            "queued and failed inside the gap, and still ours"
        );
        assert!(
            !scope.includes(&build("fonts", 7, SUCCESSFUL)),
            "what was already there is not"
        );
        assert!(
            !scope.includes(&build("other", 2, FAILED)),
            "nor is a build that was running before we triggered anything: a \
             stranger's failure is not this trigger's verdict"
        );
    }

    /// A build the server says it queued is in scope by name, so the answer
    /// does not depend on how fast it ran.
    #[test]
    fn explicitly_named_builds_are_followed_however_they_are_found() {
        let scope =
            WatchScope::from_history(&[build("fonts", 8, FAILED)]).with_explicit([BuildRef {
                pkgbase: "fonts".to_string(),
                number: 8,
            }]);
        assert!(scope.includes(&build("fonts", 8, FAILED)));
    }

    /// An update that reused an existing build row reports no new build, and
    /// the row predates the snapshot -- so the package's unfinished builds are
    /// adopted, or `--wait` would return before the build it is waiting for.
    #[test]
    fn a_reused_build_is_adopted_for_the_triggered_package_only() {
        let before = [
            build("fonts", 7, SUCCESSFUL),
            build("fonts", 8, ACTIVE),
            build("other", 3, ACTIVE),
        ];
        let scope = WatchScope::from_snapshot(&before).adopting("fonts");

        assert!(scope.includes(&build("fonts", 8, ACTIVE)));
        assert!(!scope.includes(&build("other", 3, ACTIVE)));
        assert!(
            !scope.includes(&build("fonts", 7, SUCCESSFUL)),
            "adoption is for work still in flight, not for the package's history"
        );
    }
}
