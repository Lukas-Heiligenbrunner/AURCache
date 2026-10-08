//! The build fleet, and the approval that lets a machine into it.
//!
//! A worker enrolls itself and then waits: it holds no signed certificate and
//! can claim no jobs until an operator approves it. That gate is the point of
//! this screen, so a machine waiting on it is called out rather than left to be
//! spotted in a status column.
//!
//! Revoking is not a delete. It refuses the certificate, releases whatever the
//! worker had reserved and requeues its in-flight builds — and the row stays,
//! so a build from last year still resolves to the machine that produced it.
//! That is also why retired workers are hidden behind a toggle: the list only
//! grows.

use crate::dates::RelativeDate;
use crate::format::now_secs;
use crate::listing::{ListHeader, ViewParams};
use crate::routes::Route;
use aurcache_client::{ApprovalStatus, Worker, WorkerJoinInfo};
use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;
use std::collections::HashSet;

/// Columns that only appear once there is room for them.
const WIDE_ONLY: &str = "hidden lg:table-cell";

#[component]
pub fn Workers() -> Element {
    let mut reload = use_signal(|| 0u32);
    let workers = use_resource(move || async move {
        // Read so an approve or revoke refetches the list.
        let _ = reload();
        crate::api::client()?
            .list_workers()
            .await
            .map_err(|e| e.to_string())
    });

    // Keep the fleet view live — a worker that just enrolled, went offline or
    // picked up a build should appear without a reload. Quick tick while any
    // worker is building or waiting on approval, a slow one otherwise.
    let poll_fast = matches!(&*workers.read_unchecked(), Some(Ok(list))
        if list.iter().any(|w| w.active_builds > 0 || w.status == ApprovalStatus::Pending));
    crate::poll::use_poll(workers, poll_fast);

    // The names the "reserved for" column resolves against: an entry that names
    // a package here is a link to it, one that does not is plain text. The set
    // refreshes with the list (so approve/revoke refetch it) and when a package
    // lands (so a reservation made for it links right away).
    let known_packages = use_resource(move || async move {
        let _ = reload();
        crate::api::client()?
            .list_packages(&aurcache_client::PackageQuery {
                dependencies: aurcache_client::DependencySelection::Include,
                ..Default::default()
            })
            .await
            .map(|pkgs| pkgs.into_iter().map(|p| p.name).collect::<HashSet<_>>())
            .map_err(|e| e.to_string())
    });
    crate::poll::use_refetch_on_package_change(known_packages);

    let mut show_retired = use_signal(|| false);
    let mut busy = use_signal(|| Option::<i32>::None);
    let mut status = use_signal(|| Option::<(String, bool)>::None);

    // Every action is the same shape, and every one must refetch: the server
    // decides what a worker becomes, and the row has to show what it decided.
    let act = move |(id, action): (i32, WorkerAction)| async move {
        busy.set(Some(id));
        status.set(None);
        let outcome = action.perform(id).await;
        busy.set(None);
        match outcome {
            Ok(()) => {
                status.set(Some((action.done().to_string(), true)));
                reload += 1;
            }
            Err(e) => status.set(Some((e, false))),
        }
    };

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Workers" }
                p { class: "text-xs opacity-60 max-w-prose -mt-1",
                    "A worker cannot build until it is approved. Revoking refuses its certificate, releases anything it reserved and requeues its builds; the row is kept so old builds still name the machine that ran them."
                }

                if let Some((message, ok)) = status() {
                    div {
                        class: if ok { "alert alert-success text-sm" } else { "alert alert-error text-sm" },
                        span { "{message}" }
                    }
                }

                match &*workers.read_unchecked() {
                    None => rsx! {
                        div { class: "flex justify-center p-8",
                            span { class: "loading loading-spinner loading-lg" }
                        }
                    },
                    Some(Err(e)) => rsx! {
                        div { class: "alert alert-error", span { "Could not load workers: {e}" } }
                    },
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        AddFirstWorker {}
                    },
                    Some(Ok(list)) => {
                        let waiting = list.iter().filter(|w| w.status == ApprovalStatus::Pending).count();
                        let retired = list.iter().filter(|w| w.status.is_retired()).count();
                        let shown: Vec<Worker> = list
                            .iter()
                            .filter(|w| show_retired() || !w.status.is_retired())
                            .cloned()
                            .collect();
                        // The package names in `known` are what a reservation
                        // can be resolved to. Missing or erroring leaves the
                        // column as plain text, which is the fallback anyway.
                        // One clone per render into the table props; the row
                        // loop then only runs `contains` on it.
                        let known_packages = known_packages.read();
                        let known = known_packages
                            .as_ref()
                            .and_then(|result| result.as_ref().ok());

                        rsx! {
                            div { class: "flex items-center gap-3 flex-wrap",
                                // The whole reason to open this page. A count
                                // in a status column is easy to walk past.
                                if waiting > 0 {
                                    div { class: "alert alert-warning text-sm py-2 w-auto",
                                        span { {waiting_message(waiting)} }
                                    }
                                }
                                div { class: "flex-1" }
                                if retired > 0 {
                                    button {
                                        class: "btn btn-ghost btn-xs",
                                        onclick: move |_| show_retired.toggle(),
                                        if show_retired() {
                                            "Hide retired ({retired})"
                                        } else {
                                            "Show retired ({retired})"
                                        }
                                    }
                                }
                            }
                            WorkersTable { workers: shown, fleet: list.clone(), busy: busy(), act, known: known.cloned() }
                        }
                    },
                }
            }
        }
    }
}

/// How many machines are waiting on an operator.
fn waiting_message(waiting: usize) -> String {
    match waiting {
        1 => "1 worker is waiting for approval.".to_string(),
        n => format!("{n} workers are waiting for approval."),
    }
}

/// The host the browser reached this page on, for the enrol command's
/// `AURCACHE_URL`. The worker protocol is assumed to sit on the same host — it
/// does in every ordinary deployment, and the reader can see and edit the line
/// if theirs is one of the exceptions.
fn server_host() -> Option<String> {
    web_sys::window()?
        .location()
        .hostname()
        .ok()
        .filter(|h| !h.is_empty())
}

/// A host that resolves to the browser's own machine. A worker container's own
/// `localhost` is itself, not the host, so a command built from one of these
/// needs `--network host` to reach the server.
fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
}

/// The one-liner shown on an empty Workers page: pull the image, point it at
/// this server, run it. No volume (the image declares one), no enrollment token
/// (the operator approves it here).
///
/// A loopback host gets `--network host` so a same-machine worker can reach the
/// server; a routable address is left as-is.
fn join_command(host: &str, info: &WorkerJoinInfo) -> String {
    let net = if is_loopback(host) {
        " --network host"
    } else {
        ""
    };
    format!(
        "docker run -d --privileged --tmpfs /run{net} \\\n  \
         -e AURCACHE_URL=https://{host}:{port} \\\n  {image}",
        port = info.worker_port,
        image = info.image,
    )
}

/// Shown in place of the workers table when nothing has enrolled: what a worker
/// is, and a command that adds one.
#[component]
fn AddFirstWorker() -> Element {
    let info = use_resource(|| async move {
        crate::api::client()?
            .worker_join_info()
            .await
            .map_err(|e| e.to_string())
    });

    rsx! {
        div { class: "space-y-3 py-2 max-w-prose",
            p { class: "text-sm",
                "No workers have enrolled yet. A worker is a machine that builds \
                 packages in a clean chroot and uploads them — run this on any \
                 Linux host with Docker:"
            }
            match &*info.read_unchecked() {
                None => rsx! {
                    div { class: "flex justify-center p-4",
                        span { class: "loading loading-spinner" }
                    }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error text-sm",
                        span { "Could not build the command: {e}" }
                    }
                },
                Some(Ok(info)) => {
                    let host = server_host();
                    let loopback = host.as_deref().is_some_and(is_loopback);
                    let cmd = join_command(host.as_deref().unwrap_or("YOUR_SERVER"), info);
                    rsx! {
                        pre {
                            class: "bg-base-200 rounded p-3 text-xs overflow-x-auto whitespace-pre",
                            code { class: "font-mono", "{cmd}" }
                        }
                        p { class: "text-xs opacity-60",
                            if host.is_none() {
                                "Set "
                                code { class: "font-mono", "YOUR_SERVER" }
                                " to an address the worker's machine can reach this server on. "
                            } else if loopback {
                                code { class: "font-mono", "--network host" }
                                " lets the container reach the server on this machine; to run a "
                                "worker elsewhere, drop it and use this server's address instead. "
                            }
                            "It appears here as "
                            span { class: "badge badge-warning badge-sm align-middle", "pending" }
                            " within a few seconds; approve it and it starts taking builds."
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn WorkersTable(
    workers: Vec<Worker>,
    /// Every worker there is, retired ones included.
    ///
    /// Beside the rows actually shown, because a link has to know whether a
    /// name is unique across the *whole* fleet: the row a shared name collides
    /// with is most often the retired one this list is hiding.
    fleet: Vec<Worker>,
    /// The worker with an action in flight, if any.
    busy: Option<i32>,
    act: EventHandler<(i32, WorkerAction)>,
    /// Package names that resolve to a real package. A reservation naming one
    /// is a link; any other reservation stays plain text. `None` while the
    /// package list is still loading or failed — plain text is the fallback
    /// anyway.
    known: Option<HashSet<String>>,
) -> Element {
    // Read once for the whole table rather than per row, so every "3m ago" on
    // screen is measured from the same instant.
    let now = now_secs();

    // Denominator for each worker's share. Finished builds only, and only
    // those a worker owns: builds from before the worker split carry no
    // worker_id, and counting them would shrink everyone's share against work
    // no worker present did. Over the whole `fleet`, not the filtered rows:
    // with retired workers hidden (the default) the visible rows would
    // otherwise divide by a smaller total and every share would read high.
    let fleet_finished: i32 = fleet
        .iter()
        .map(|w| w.successful_builds + w.failed_builds)
        .sum();

    // Counted once per table, not once per row: each row's link needs to
    // know whether its name is shared.
    let shared_names = crate::screens::worker::shared_names(&fleet);

    rsx! {
        div { class: "overflow-x-auto",
            table { class: "table table-zebra",
                thead {
                    tr {
                        th { "Worker" }
                        th { "Status" }
                        th { "Now" }
                        th { class: "{WIDE_ONLY}", "Builds" }
                        th { "Architectures" }
                        th { class: "{WIDE_ONLY}", "Reserved for" }
                        th { class: "{WIDE_ONLY}", "Priority" }
                        th { class: "{WIDE_ONLY}", "Type" }
                        th { class: "{WIDE_ONLY}", "Last seen" }
                        th { "" }
                    }
                }
                tbody {
                    for worker in workers.iter() {
                        // Computed once per row for the name link, from
                        // the table-wide shared set above rather than a fresh
                        // fleet scan per row.
                        {
                        let route = crate::screens::worker::worker_route(
                            worker,
                            shared_names.contains(worker.name.as_str()),
                        );
                        rsx! {
                        tr { key: "{worker.id}", class: if worker.status.is_retired() { "opacity-50" } else { "" },
                            td {
                                // The fingerprint is the identity; the name is
                                // whatever the machine called itself and is not
                                // unique.
                                div { class: "font-mono text-sm flex items-center gap-1",
                                    // The name is the way in. The fingerprint
                                    // is the identity behind it; the name is
                                    // whatever the machine called itself and is
                                    // not unique.
                                    Link {
                                        class: "link-hover",
                                        title: "{worker.cert_fingerprint}",
                                        to: route,
                                        "{worker.name}"
                                    }
                                    // Something this machine was configured
                                    // with is not what it is running. Shown
                                    // here rather than only inside the panel:
                                    // nobody opens a panel they have no reason
                                    // to suspect.
                                    if worker.settings_rejected.is_some_and(|n| n > 0) {
                                        span {
                                            class: "text-warning",
                                            title: "This worker refused a configured value; open it to see which.",
                                            "\u{26a0}"
                                        }
                                    }
                                }
                            }
                            td {
                                div { class: "flex flex-wrap gap-1",
                                    StatusBadge { status: worker.status }
                                    if worker.paused {
                                        PausedBadge {}
                                    }
                                }
                            }
                            td { Liveness { worker: worker.clone() } }
                            td { class: "{WIDE_ONLY}",
                                Record { worker: worker.clone(), fleet_finished }
                            }
                            td { class: "text-sm", {architectures(worker)} }
                            td { class: "{WIDE_ONLY}",
                                if worker.package_affinity.is_empty() {
                                    span { class: "opacity-40", "—" }
                                } else {
                                    div { class: "flex flex-wrap gap-1",
                                        for package in worker.package_affinity.iter() {
                                            if known.as_ref().is_some_and(|known| known.contains(package)) {
                                                Link {
                                                    key: "{package}",
                                                    title: "Open package page",
                                                    class: "badge badge-outline badge-sm font-mono link-hover",
                                                    to: Route::Package { pkgbase: package.clone() },
                                                    "{package}"
                                                }
                                            } else {
                                                span { key: "{package}", class: "badge badge-outline badge-sm font-mono", "{package}" }
                                            }
                                        }
                                    }
                                }
                            }
                            td { class: "{WIDE_ONLY} text-sm",
                                if worker.priority == 0 {
                                    // Zero is the default, meaning no
                                    // preference. Printing it suggests the
                                    // fleet has been tuned when it has not.
                                    span { class: "opacity-40", "—" }
                                } else {
                                    span {
                                        title: "Higher priority workers are offered jobs first.",
                                        "{worker.priority}"
                                    }
                                }
                            }
                            td { class: "{WIDE_ONLY}",
                                KindBadge { worker: worker.clone() }
                            }
                            td { class: "{WIDE_ONLY} text-sm opacity-70",
                                if worker.last_seen.is_some() {
                                    RelativeDate { ts: worker.last_seen, now }
                                } else {
                                    // Enrolled but never called in — different
                                    // from "a long time ago", and the case an
                                    // operator is usually looking at.
                                    span { class: "opacity-60", "never" }
                                }
                            }
                            td {
                                WorkerActions {
                                    worker: worker.clone(),
                                    busy: busy == Some(worker.id),
                                    act,
                                }
                            }
                        }
                        }
                        }
                    }
                }
            }
        }
    }
}

/// What can be done to a worker, given where it is in the workflow.
///
/// Approve appears for anything not already approved — including a revoked
/// worker, since letting a machine back in is the same act as letting it in the
/// first time. Revoke disappears once revoked, because there is nothing left to
/// take away.
#[component]
fn WorkerActions(worker: Worker, busy: bool, act: EventHandler<(i32, WorkerAction)>) -> Element {
    let id = worker.id;
    rsx! {
        div { class: "flex gap-2 justify-end",
            if busy {
                span { class: "loading loading-spinner loading-xs" }
            }
            if !worker.status.can_build() {
                button {
                    class: "btn btn-primary btn-xs",
                    disabled: busy,
                    onclick: move |_| act.call((id, WorkerAction::Approve)),
                    if worker.status.is_retired() { "Reinstate" } else { "Approve" }
                }
            }
            if worker.status.can_build() {
                if worker.paused {
                    button {
                        class: "btn btn-ghost btn-xs",
                        disabled: busy,
                        title: WorkerAction::Resume.hint(),
                        onclick: move |_| act.call((id, WorkerAction::Resume)),
                        "Resume intake"
                    }
                } else {
                    button {
                        class: "btn btn-ghost btn-xs",
                        disabled: busy,
                        title: WorkerAction::Pause.hint(),
                        onclick: move |_| act.call((id, WorkerAction::Pause)),
                        "Stop intake"
                    }
                }
            }
            if !worker.status.is_retired() {
                button {
                    class: "btn btn-ghost btn-xs",
                    disabled: busy,
                    title: WorkerAction::Revoke.hint(),
                    onclick: move |_| act.call((id, WorkerAction::Revoke)),
                    "Revoke"
                }
            }
        }
    }
}

/// Something an operator does to a worker from the fleet list or its page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerAction {
    Approve,
    Revoke,
    /// Take no new builds; let the ones it holds finish.
    Pause,
    Resume,
}

impl WorkerAction {
    pub(crate) async fn perform(self, id: i32) -> Result<(), String> {
        let client = crate::api::client()?;
        match self {
            Self::Approve => client.approve_worker(id).await,
            Self::Revoke => client.revoke_worker(id).await,
            Self::Pause => client.pause_worker(id).await,
            Self::Resume => client.resume_worker(id).await,
        }
        .map_err(|e| e.to_string())
    }

    /// What has happened, once the server has done it.
    pub(crate) const fn done(self) -> &'static str {
        match self {
            Self::Approve => "Worker approved. It can claim jobs once it checks in.",
            Self::Revoke => "Worker revoked. Its builds have been requeued.",
            Self::Pause => {
                "Intake stopped. New builds go to other workers; the ones it is running \
                 finish."
            }
            Self::Resume => "Intake resumed. The worker takes new builds from its next claim.",
        }
    }

    /// What the button does, for its tooltip.
    pub(crate) const fn hint(self) -> &'static str {
        match self {
            Self::Approve => "Let this worker claim builds",
            Self::Revoke => "Refuse this worker's certificate and requeue its builds",
            Self::Pause => {
                "Stop giving this worker new builds; they go to other workers. Builds it is \
                 running finish -- for a reboot, an upgrade or retiring the machine"
            }
            Self::Resume => "Give this worker new builds again",
        }
    }
}

/// An approved worker that has been asked to take nothing new.
#[component]
pub(crate) fn PausedBadge() -> Element {
    rsx! {
        span {
            class: "badge badge-info badge-soft badge-sm whitespace-nowrap",
            title: "New builds go to other workers; the ones it is running finish",
            "intake stopped"
        }
    }
}

#[component]
pub(crate) fn StatusBadge(status: ApprovalStatus) -> Element {
    let (label, class) = match status {
        ApprovalStatus::Approved => ("approved", "badge-success"),
        ApprovalStatus::Pending => ("pending", "badge-warning"),
        ApprovalStatus::Revoked => ("revoked", "badge-neutral"),
    };
    rsx! {
        span { class: "badge {class} badge-sm whitespace-nowrap", "{label}" }
    }
}

/// How a worker describes itself: its build strategy, at the version running it.
///
/// Joined with `@`, the "artifact at version" spelling npm and Go established,
/// so it reads as one token without a legend. Not `:`, which would be actively
/// misleading here -- `docker` is itself one of the kinds, so `docker:0.5.0`
/// reads as an image tag rather than as a kind at a version. Not `/`, which
/// reads as a path segment.
///
/// The two halves are independently absent. A worker that enrolled before
/// either was reported has neither; one that predates only the kind still has a
/// version worth showing, so that case renders the version alone rather than a
/// stray separator.
fn kind_label(worker: &Worker) -> Option<String> {
    match (worker.kind.as_deref(), worker.version.as_deref()) {
        (Some(kind), Some(version)) => Some(format!("{kind}@{version}")),
        (Some(kind), None) => Some(kind.to_string()),
        (None, Some(version)) => Some(version.to_string()),
        (None, None) => None,
    }
}

/// Which build strategy a worker runs, and at what version.
///
/// The kind is whatever the worker called itself, so this renders an unknown
/// value rather than falling back to a default -- a new executor should show up
/// on this page without the frontend having been taught about it.
///
/// The one exception is the legacy container builder, which is deprecated and
/// worth flagging: "which of my workers are still on it" is the question an
/// operator asks before removing it.
#[component]
pub(crate) fn KindBadge(worker: Worker) -> Element {
    let Some(label) = kind_label(&worker) else {
        return rsx! {
            span {
                class: "opacity-40",
                title: "This worker enrolled before workers reported a type; restarting it fills this in.",
                "—"
            }
        };
    };
    let (class, title) = if worker.kind.as_deref() == Some("docker") {
        (
            "badge-warning",
            "The legacy container builder, which is deprecated. Migrate to the chroot worker.",
        )
    } else {
        (
            "badge-ghost",
            "The build strategy this worker reported, and the version it runs.",
        )
    };
    rsx! {
        span { class: "badge {class} badge-sm font-mono whitespace-nowrap", title: "{title}", "{label}" }
    }
}

/// What a worker can build, and how.
///
/// Emulated architectures are marked rather than listed alongside the native
/// ones: they work, but slowly, and a fleet that looks like it has four native
/// aarch64 machines when it has none is worth not implying.
pub(crate) fn architectures(worker: &Worker) -> String {
    match (
        worker.native_arches.is_empty(),
        worker.emulated_arches.is_empty(),
    ) {
        (true, true) => "—".to_string(),
        (false, true) => worker.native_arches.join(", "),
        (true, false) => format!("emulated: {}", worker.emulated_arches.join(", ")),
        (false, false) => format!(
            "{} (emulated: {})",
            worker.native_arches.join(", "),
            worker.emulated_arches.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        StatusBadge, StatusBadgeProps, architectures, is_loopback, join_command, kind_label,
        waiting_message,
    };
    use aurcache_client::{ApprovalStatus, Worker, WorkerJoinInfo};
    use dioxus::prelude::*;

    fn worker(native: &[&str], emulated: &[&str]) -> Worker {
        Worker {
            id: 1,
            name: "builder".to_string(),
            status: ApprovalStatus::Approved,
            cert_fingerprint: "abc".to_string(),
            native_arches: native.iter().map(ToString::to_string).collect(),
            emulated_arches: emulated.iter().map(ToString::to_string).collect(),
            package_affinity: Vec::new(),
            priority: 0,
            last_seen: None,
            version: None,
            kind: None,
            // These tests are about how architectures are described; the
            // liveness and record columns have their own below.
            online: false,
            active_builds: 0,
            successful_builds: 0,
            failed_builds: 0,
            settings_rejected: None,
            paused: false,
        }
    }

    /// One column, so the two values are joined rather than shown side by side.
    #[test]
    fn the_type_column_names_the_kind_at_its_version() {
        let mut w = worker(&["x86_64"], &[]);
        w.kind = Some("chroot".to_string());
        w.version = Some("0.5.0".to_string());
        assert_eq!(kind_label(&w).as_deref(), Some("chroot@0.5.0"));
    }

    /// Each half can be missing on its own, and a worker that reports a version
    /// but no kind is not hypothetical -- enrolments from before kind reporting
    /// look exactly like that. Neither case may render a dangling separator.
    #[test]
    fn a_missing_half_does_not_leave_a_separator() {
        let mut w = worker(&["x86_64"], &[]);

        w.kind = Some("chroot".to_string());
        w.version = None;
        assert_eq!(kind_label(&w).as_deref(), Some("chroot"));

        w.kind = None;
        w.version = Some("0.1.0".to_string());
        assert_eq!(kind_label(&w).as_deref(), Some("0.1.0"));

        // Nothing reported at all is the em dash the cell renders itself.
        w.version = None;
        assert_eq!(kind_label(&w), None);
    }

    /// Emulation works but is slow. A fleet that reads as four native aarch64
    /// machines when it has none would be a misleading thing to imply.
    #[test]
    fn emulated_architectures_are_marked_as_such() {
        assert_eq!(architectures(&worker(&["x86_64"], &[])), "x86_64");
        assert_eq!(
            architectures(&worker(&["x86_64"], &["aarch64"])),
            "x86_64 (emulated: aarch64)"
        );
        assert_eq!(
            architectures(&worker(&[], &["aarch64"])),
            "emulated: aarch64"
        );
        assert_eq!(architectures(&worker(&[], &[])), "—");
    }

    #[test]
    fn the_waiting_notice_counts_properly() {
        assert_eq!(waiting_message(1), "1 worker is waiting for approval.");
        assert_eq!(waiting_message(3), "3 workers are waiting for approval.");
    }

    /// For a routable host the command points at it on the server's reported
    /// worker port, pulls the server's reported image, and carries nothing else
    /// — no `-v`, no token, no `--network host`.
    #[test]
    fn the_join_command_is_host_port_and_image_only() {
        let info = WorkerJoinInfo {
            image: "registry.example/aurcache-worker:v1".to_string(),
            worker_port: 9443,
        };
        let cmd = join_command("build.example.com", &info);
        assert!(
            cmd.contains("AURCACHE_URL=https://build.example.com:9443"),
            "{cmd}"
        );
        assert!(cmd.contains("registry.example/aurcache-worker:v1"), "{cmd}");
        assert!(cmd.contains("--privileged"), "{cmd}");
        assert!(!cmd.contains("-v "), "no volume flag: {cmd}");
        assert!(!cmd.contains("TOKEN"), "no enrollment token: {cmd}");
        assert!(!cmd.contains("--network"), "routable host: {cmd}");
    }

    /// A loopback host is the browser's machine, not the worker container's, so
    /// the command shares the host's network to bridge the gap — and keeps the
    /// loopback address, which the default TLS SAN covers.
    #[test]
    fn the_join_command_adds_host_networking_for_loopback() {
        assert!(is_loopback("localhost"));
        assert!(is_loopback("127.0.0.1"));
        assert!(!is_loopback("aur.example.com"));

        let info = WorkerJoinInfo {
            image: "img".to_string(),
            worker_port: 8083,
        };
        let cmd = join_command("localhost", &info);
        assert!(cmd.contains("--network host"), "{cmd}");
        assert!(cmd.contains("AURCACHE_URL=https://localhost:8083"), "{cmd}");
    }

    fn actions_for(status: ApprovalStatus, paused: bool) -> String {
        #[component]
        fn Harness(worker: Worker) -> Element {
            rsx! {
                super::WorkerActions {
                    worker,
                    busy: false,
                    act: move |_| {},
                }
            }
        }
        let mut w = worker(&["x86_64"], &[]);
        w.status = status;
        w.paused = paused;
        let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { worker: w });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// Pause is offered to an approved worker, Resume to a paused one, and
    /// neither to a worker that claims nothing anyway.
    #[test]
    fn drain_and_resume_follow_the_workers_state() {
        let approved = actions_for(ApprovalStatus::Approved, false);
        assert!(
            approved.contains(">Stop intake<") && !approved.contains(">Resume intake<"),
            "{approved}"
        );
        let paused = actions_for(ApprovalStatus::Approved, true);
        assert!(
            paused.contains(">Resume intake<") && !paused.contains(">Stop intake<"),
            "{paused}"
        );
        for status in [ApprovalStatus::Pending, ApprovalStatus::Revoked] {
            let html = actions_for(status, false);
            assert!(
                !html.contains(">Stop intake<") && !html.contains(">Resume intake<"),
                "{status:?}: {html}"
            );
        }
    }

    /// The three states have to be distinguishable at a glance; pending is the
    /// one that wants an operator, so it is the one that must not read as calm.
    #[test]
    fn each_status_gets_its_own_badge() {
        for (status, label, class) in [
            (ApprovalStatus::Approved, "approved", "badge-success"),
            (ApprovalStatus::Pending, "pending", "badge-warning"),
            (ApprovalStatus::Revoked, "revoked", "badge-neutral"),
        ] {
            let mut dom = VirtualDom::new_with_props(StatusBadge, StatusBadgeProps { status });
            dom.rebuild_in_place();
            let html = dioxus_ssr::render(&dom);
            assert!(html.contains(label), "{status:?}: {html}");
            assert!(html.contains(class), "{status:?}: {html}");
        }
    }
}

/// Why the dot is the colour it is.
fn liveness_hint(online: bool) -> &'static str {
    if online {
        "Checked in within the liveness timeout"
    } else {
        "Has not checked in recently; jobs will spill to other workers"
    }
}

/// Whether a worker is connected, and what it is doing right now.
///
/// Connectivity is not the approval status beside it: an approved worker that
/// stopped calling in still reads "approved", and that column would go on
/// saying so for as long as the machine stayed off. This is the column that
/// answers whether the fleet is actually there.
#[component]
pub(crate) fn Liveness(worker: Worker) -> Element {
    // A revoked worker is not expected to be connected, so absence is not
    // worth reporting as though something were wrong.
    if worker.status.is_retired() {
        return rsx! { span { class: "opacity-40", "—" } };
    }

    rsx! {
        div { class: "flex items-center gap-2 text-sm",
            span {
                class: if worker.online { "badge badge-success badge-xs" } else { "badge badge-outline badge-xs opacity-40" },
                title: liveness_hint(worker.online),
            }
            if worker.active_builds > 0 {
                // Straight to what this machine is building, rather than to
                // every build it has ever run with the filter still to apply.
                Link {
                    class: "whitespace-nowrap hover:underline",
                    to: Route::Builds {
                        view: ViewParams::with_status(BuildState::Active),
                        q: worker.name.clone(),
                    },
                    title: "Show what this worker is building",
                    "{worker.active_builds} building"
                }
            } else if worker.online {
                span { class: "opacity-60", "idle" }
            } else {
                span { class: "opacity-60", "offline" }
            }
        }
    }
}

/// How much attention a success rate deserves.
///
/// Not "any failure at all": a machine that has failed one build in a hundred
/// is working, and colouring 99% as a warning spends the reader's attention on
/// the wrong row. The threshold is where a rate stops reading as noise.
fn rate_class(rate: f64) -> &'static str {
    if rate < 50.0 {
        "text-error"
    } else if rate < 90.0 {
        "text-warning"
    } else {
        ""
    }
}

/// What a worker has actually produced.
///
/// Counts rather than a bare rate: three of three is not the same evidence as
/// three hundred of three hundred, and a rate alone hides which one you have.
/// The share says whether this machine matters to the fleet — a worker that is
/// reliable but takes one build in fifty is a different thing to fix than one
/// that takes half of them and fails.
#[component]
fn Record(worker: Worker, fleet_finished: i32) -> Element {
    let finished = worker.successful_builds + worker.failed_builds;
    // Filtering is by name because that is what the Builds list matches on and
    // what it shows in its own Worker column, so the destination explains the
    // filter it arrived with. A name is not unique the way the fingerprint is;
    // two machines sharing one would list together, which is a truthful answer
    // to "builds from a worker called this" and the same answer the Builds page
    // gives to anyone typing it into the search box.
    let filter = Route::Builds {
        view: ViewParams::default(),
        q: worker.name.clone(),
    };
    if finished == 0 {
        return rsx! {
            Link {
                class: "text-sm opacity-40 hover:opacity-70",
                to: filter,
                // Still a link with nothing finished: a worker part-way through
                // its first build has something to show, and the count here
                // only covers builds that ended.
                title: "Show this worker's builds",
                "no builds yet"
            }
        };
    }

    let rate = f64::from(worker.successful_builds) / f64::from(finished) * 100.0;
    let share = if fleet_finished > 0 {
        f64::from(finished) / f64::from(fleet_finished) * 100.0
    } else {
        0.0
    };

    rsx! {
        Link {
            class: "text-sm leading-tight block hover:underline",
            to: filter,
            title: "Show this worker's builds",
            div { class: "flex items-center gap-1 whitespace-nowrap",
                span { class: rate_class(rate), "{rate:.0}%" }
                span { class: "opacity-60", "of {finished}" }
            }
            div { class: "text-xs opacity-60 whitespace-nowrap",
                if worker.failed_builds > 0 {
                    "{worker.failed_builds} failed · "
                }
                "{share:.0}% of fleet"
            }
        }
    }
}
