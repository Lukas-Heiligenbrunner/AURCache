//! The log: what happened, when, who asked for it, and what went wrong.
//!
//! Not only what people did. The server writes an entry wherever it already
//! knows something failed -- a build that published nowhere, a worker that
//! stopped answering -- so the page you read to see what has been going on is
//! also the page that tells you something is off.
//!
//! Each entry is a typed event, rendered here from its payload so that what it
//! names links to its page; an entry this build cannot decode -- written by a
//! newer server -- shows the sentence stored with it instead.
//!
//! The one list in this app filtered *server-side*. Everything else fetches
//! its rows and filters them in the browser (see `crate::listing`); the log
//! cannot be fetched whole, because it only grows, and a filter applied to one
//! page would only ever search the page you were already looking at.

use crate::dates::AbsoluteDate;
use crate::listing::{LIST_TABLE, ListHeader, PAGE_SIZE, Pager, ViewParams, use_url_view};
use crate::routes::Route;
use aurcache_client::{
    ActiveOperation, EntityRef, Event, KINDS, LogEntry, LogFilter, PackageRef, Segment, Severity,
    WorkerRef, kind_label, operation_kind,
};
use dioxus::prelude::*;
use std::time::Duration;

/// How much of the log to fetch at a time.
///
/// The same page size the other lists use, but fetched a page at a time rather
/// than sliced from everything: the log only grows, so there is no point at
/// which asking for all of it is the cheap option.
const PAGE: u64 = PAGE_SIZE as u64;

/// How often to re-ask what is still running.
///
/// Slower than a progress card's own polling: this list only reports *that*
/// something is running, and a job showing up a second late costs nothing.
const RUNNING_POLL: Duration = Duration::from_secs(3);

#[component]
pub fn Logs(view: ViewParams) -> Element {
    let mut page = use_signal(|| 0usize);
    let severity = use_signal(|| view.severity);
    let since_boot = use_signal(|| view.since_boot);
    let mut about = use_signal(|| view.about.clone());
    let mut kind = use_signal(|| view.kind.clone());

    // The whole route from live control state, which is what keeps a filtered
    // log linkable and survives a reload. The search box picks a filter rather
    // than holding a term, so there is no term signal.
    let to_route = move |_: String| Route::Logs {
        view: ViewParams::for_logs(severity(), since_boot(), about()).with_kind(kind()),
    };
    use_url_view(None, true, to_route);

    // Narrowing the log changes what page 1 even is, so start again from it --
    // otherwise a filter applied on page 4 lands past the end of a shorter log.
    use_effect(use_reactive(
        &(severity(), since_boot(), about(), kind()),
        move |_| {
            page.set(0);
        },
    ));

    let entries = use_resource(move || async move {
        let offset = page() as u64 * PAGE;
        let query = LogFilter {
            severity: severity(),
            since_boot: since_boot(),
            entity: about(),
            kind: kind(),
            ..LogFilter::default()
        };
        crate::api::client()?
            .log(Some(PAGE), Some(offset), &query)
            .await
            .map_err(|e| e.to_string())
    });

    rsx! {
        div { class: "space-y-4",

        RunningOperations {}

        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Logs" }
                LogControls { severity, since_boot, about, kind }

                match &*entries.read_unchecked() {
                    None => rsx! {
                        div { class: "flex justify-center p-8",
                            span { class: "loading loading-spinner loading-lg" }
                        }
                    },
                    Some(Err(e)) => rsx! {
                        div { class: "alert alert-error", span { "Could not load the log: {e}" } }
                    },
                    Some(Ok(view)) if view.total == 0 => rsx! {
                        div { class: "alert",
                            // A filter that matched nothing is a different
                            // answer from an empty log, and the remedy is
                            // different too.
                            if severity().is_some() || since_boot() || about().is_some() || kind().is_some() {
                                span { "Nothing matches these filters." }
                            } else {
                                span { "Nothing has happened yet." }
                            }
                        }
                    },
                    Some(Ok(view)) => rsx! {
                        LogTable {
                            entries: view.entries.clone(),
                            on_about: move |entity| about.set(Some(entity)),
                            on_kind: move |chosen| kind.set(Some(chosen)),
                        }
                        Pager {
                            page,
                            index: page().min(pages_of(view.total).saturating_sub(1)),
                            pages: pages_of(view.total),
                            first: page() * PAGE_SIZE,
                            count: view.entries.len(),
                            total: view.total as usize,
                        }
                    },
                }
            }
        }
        }
    }
}

/// Long-running work still in flight, with a way back to its progress card.
///
/// A bulk add outlives the dialog that started it and the browser that was
/// watching, so without this a job whose card was dismissed -- or which was
/// started from another tab, or before a reload -- is running with nothing on
/// screen to say so. The server knows; this is what asks it.
///
/// Absent entirely when nothing is running: a permanent empty panel on a page
/// about history would be noise.
#[component]
fn RunningOperations() -> Element {
    let jobs = crate::progress::use_jobs();

    // Polled rather than fetched once: this page is somewhere to leave open
    // while something runs, and a list that went stale the moment it loaded
    // would be worse than not having it.
    //
    // One loop that sleeps *after* each fetch, not a timer beside it: a
    // restart timer fires whether or not the previous fetch finished, so on
    // a slow network it cancels every fetch and the list starves. Here the
    // next fetch starts one interval after the last one completed.
    let mut list = use_signal(Vec::new);
    // Whether the last fetch failed: an empty list is ambiguous otherwise —
    // nothing running reads exactly like a dead server.
    let mut failed = use_signal(|| false);
    use_future(move || async move {
        loop {
            // No fetch while hidden: a build server sitting in a background
            // tab should not emit requests, same rule as `use_poll`. The
            // sleep below keeps ticking so the return is noticed.
            if !crate::poll::hidden()
                && let Ok(client) = crate::api::client()
                && let Ok(running) = client.active_operations().await
            {
                list.set(running);
                failed.set(false);
            } else if !crate::poll::hidden() {
                failed.set(true);
            }
            gloo_timers::future::sleep(RUNNING_POLL).await;
        }
    });

    let list = list.read().clone();
    if list.is_empty() {
        return if failed() {
            rsx! {
                div { class: "alert alert-warning",
                    span { "Could not reach the server — operations may be running unseen." }
                }
            }
        } else {
            rsx! {}
        };
    }

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Running now" }
                div { class: "space-y-2",
                    for operation in list {
                        RunningRow { key: "{operation.id}", operation, jobs }
                    }
                }
            }
        }
    }
}

/// One running operation.
#[component]
fn RunningRow(operation: ActiveOperation, jobs: Signal<Vec<crate::progress::Job>>) -> Element {
    let resolved = operation.resolved_count();
    let watching = jobs
        .read()
        .iter()
        .any(|job| job.operation_id() == Some(operation.id));

    // Both kinds have a card; they differ in what they poll and in the words
    // they report outcomes with. A kind this build does not know has neither,
    // so it gets no button rather than one that would open an empty card.
    let known = matches!(
        operation.kind.as_str(),
        operation_kind::BULK_ADD | operation_kind::RESTORE
    );
    let label = match operation.kind.as_str() {
        operation_kind::BULK_ADD => format!("Adding {} packages", operation.total),
        operation_kind::RESTORE => format!("Restoring {} packages", operation.total),
        operation_kind::DEPENDENCY_BACKFILL => {
            format!("Recording dependencies of {} packages", operation.total)
        }
        // A kind this build does not know about, from a newer server. Saying so
        // beats hiding it.
        other => format!("{other} ({} items)", operation.total),
    };

    rsx! {
        div { class: "flex items-center gap-3",
            span { class: "loading loading-spinner loading-xs" }
            div { class: "flex-1 min-w-0",
                div { class: "text-sm truncate", "{label}" }
                div { class: "text-xs opacity-60",
                    "{resolved} of {operation.total}"
                    if operation.failed > 0 {
                        ", {operation.failed} failed"
                    }
                }
            }
            if known {
                if watching {
                    span { class: "text-xs opacity-60", "shown" }
                } else {
                    button {
                        class: "btn btn-ghost btn-xs",
                        onclick: move |_| {
                            let (id, total, label) = (operation.id, operation.total, label.clone());
                            if operation.kind == operation_kind::RESTORE {
                                crate::progress::watch_restore(jobs, id, total, label);
                            } else {
                                crate::progress::watch_add(jobs, id, total, label);
                            }
                        },
                        "Show progress"
                    }
                }
            }
        }
    }
}

/// An entry's sentence, with everything it names linked to its page.
///
/// Rendered from the payload rather than the stored text, which is what lets
/// each name be a link without the server laying out a page. The stored text
/// is the fallback, for an entry this build cannot decode.
///
/// A build is two links, the package and the number: "hello #7" is about both,
/// and the package page is as likely a next step as the build's.
#[component]
pub(crate) fn EntryText(entry: LogEntry) -> Element {
    let Some(event) = Event::decode(&entry.kind, &entry.data) else {
        return rsx! { "{entry.message}" };
    };
    rsx! {
        for segment in event.sentence() {
            match segment {
                Segment::Text(text) => rsx! { "{text}" },
                Segment::Entity(EntityRef::Build(build)) => rsx! {
                    Link {
                        class: "link-hover font-medium",
                        to: Route::Package { pkgbase: build.pkgbase.clone() },
                        "{build.pkgbase}"
                    }
                    " "
                    Link {
                        class: "link-hover font-medium",
                        to: entity_route(&EntityRef::Build(build.clone())),
                        "#{build.number}"
                    }
                },
                Segment::Entity(entity) => rsx! {
                    Link {
                        class: "link-hover font-medium",
                        to: entity_route(&entity),
                        "{entity.label()}"
                    }
                },
            }
        }
    }
}

/// How many entries a page's own activity panel shows before sending the
/// reader to the log for the rest.
const RECENT: u64 = 10;

/// The latest entries about one package or worker, with the way to all of them.
///
/// The whole history is the log narrowed to it -- for a package, back to the
/// entry that added it, within the log's retention -- so the panel shows the
/// head and links there rather than paging a second copy of the same list.
#[component]
pub fn RecentActivity(about: EntityRef) -> Element {
    let entries = use_resource(use_reactive(&about, |about| async move {
        let query = LogFilter {
            entity: Some(about),
            ..LogFilter::default()
        };
        crate::api::client()?
            .log(Some(RECENT), None, &query)
            .await
            .map_err(|e| e.to_string())
    }));
    crate::poll::use_poll(entries, false);

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                Link {
                    class: "card-title text-base link-hover w-fit",
                    to: Route::Logs {
                        view: ViewParams::about(about),
                    },
                    "Activity"
                }
                match &*entries.read_unchecked() {
                    None => rsx! {
                        span { class: "loading loading-spinner loading-sm" }
                    },
                    Some(Err(e)) => rsx! {
                        div { class: "text-sm text-error", "Could not load the activity: {e}" }
                    },
                    Some(Ok(page)) if page.entries.is_empty() => rsx! {
                        p { class: "text-sm opacity-60", "Nothing recorded yet." }
                    },
                    Some(Ok(page)) => rsx! {
                        ul { class: "divide-y divide-base-300",
                            for entry in page.entries.iter() {
                                li { key: "{entry.id}", class: "py-1.5 text-sm flex gap-3",
                                    span { class: "whitespace-nowrap opacity-60 shrink-0",
                                        AbsoluteDate { ts: Some(entry.timestamp) }
                                    }
                                    span { class: "min-w-0 break-words",
                                        SeverityBadge { severity: entry.severity }
                                        EntryText { entry: entry.clone() }
                                        if let Some(user) = &entry.user {
                                            span { class: "opacity-50", " — {user}" }
                                        }
                                    }
                                }
                            }
                        }
                        if page.total > RECENT {
                            p { class: "text-xs opacity-60",
                                "{page.total - RECENT} older "
                                if page.total - RECENT == 1 { "entry" } else { "entries" }
                                " in the log."
                            }
                        }
                    },
                }
            }
        }
    }
}

/// Where a reference opens.
///
/// Every reference links, whether or not what it names is still there: the
/// page it opens handles that, offering to add a missing package or falling
/// back to its builds for a missing build. A worker goes by name, which is how
/// its page is addressed and which offers a choice if two machines share it.
pub(crate) fn entity_route(entity: &EntityRef) -> Route {
    match entity {
        EntityRef::Package(pkg) => Route::Package {
            pkgbase: pkg.0.clone(),
        },
        EntityRef::Worker(worker) => Route::Worker {
            name: crate::screens::worker::name_segments(&worker.0),
        },
        EntityRef::Build(build) => Route::Build {
            pkgbase: build.pkgbase.clone(),
            number: build.number,
        },
    }
}

/// What to narrow the log to.
///
/// Its own controls rather than the shared [`crate::listing::ListControls`]:
/// that pair is a name search and a build status, and the log has neither. A
/// search box that did nothing would be worse than no search box.
#[component]
fn LogControls(
    severity: Signal<Option<Severity>>,
    since_boot: Signal<bool>,
    about: Signal<Option<EntityRef>>,
    kind: Signal<Option<String>>,
) -> Element {
    let mut severity = severity;
    let mut since_boot = since_boot;
    let mut about = about;
    let mut kind = kind;

    rsx! {
        div { class: "flex flex-wrap items-center gap-3 mb-2",
            select {
                class: "select select-bordered select-sm",
                aria_label: "Filter by severity",
                onchange: move |e| severity.set(Severity::from_slug(&e.value())),
                option { value: "", selected: severity().is_none(), "Everything" }
                option {
                    value: Severity::Warning.slug(),
                    selected: severity() == Some(Severity::Warning),
                    "Warnings and errors"
                }
                option {
                    value: Severity::Error.slug(),
                    selected: severity() == Some(Severity::Error),
                    "Errors only"
                }
            }
            KindSelect { kind }
            label { class: "label cursor-pointer gap-2 py-0",
                input {
                    r#type: "checkbox",
                    class: "checkbox checkbox-sm",
                    aria_label: "Since the last restart",
                    checked: since_boot(),
                    onchange: move |e| since_boot.set(e.checked()),
                }
                span { class: "label-text text-sm", "Since the last restart" }
            }
            // Narrowed to one package, build or worker: say which, and let it
            // go. Otherwise, offer to narrow it.
            if let Some(entity) = about() {
                span { class: "badge badge-lg gap-1",
                    "{kind_of(&entity)} "
                    span { class: "font-mono", "{entity.label()}" }
                    button {
                        class: "btn btn-ghost btn-xs btn-circle",
                        aria_label: "Show the whole log",
                        onclick: move |_| about.set(None),
                        "✕"
                    }
                }
            } else {
                EntitySearch { about }
            }
            // A kind from a newer server, or a hand-edited URL, is not in the
            // select; the chip still says what the log is narrowed to.
            if let Some(chosen) = kind().filter(|chosen| !KINDS.iter().any(|k| k.kind == chosen)) {
                span { class: "badge badge-lg gap-1",
                    span { class: "font-mono", "{chosen}" }
                    button {
                        class: "btn btn-ghost btn-xs btn-circle",
                        aria_label: "Show every kind",
                        onclick: move |_| kind.set(None),
                        "✕"
                    }
                }
            }
        }
    }
}

/// Narrow the log to one kind of entry, from the catalogue grouped by area.
#[component]
fn KindSelect(kind: Signal<Option<String>>) -> Element {
    let mut kind = kind;
    let mut groups: Vec<&'static str> = Vec::new();
    for known in KINDS {
        if !groups.contains(&known.group) {
            groups.push(known.group);
        }
    }
    rsx! {
        select {
            class: "select select-bordered select-sm",
            aria_label: "Filter by kind",
            onchange: move |e| kind.set(Some(e.value()).filter(|value| !value.is_empty())),
            option { value: "", selected: kind().is_none(), "Every kind" }
            for group in groups {
                optgroup { key: "{group}", label: "{group}",
                    for known in KINDS.iter().filter(|known| known.group == group) {
                        option {
                            key: "{known.kind}",
                            value: "{known.kind}",
                            selected: kind().as_deref() == Some(known.kind),
                            "{known.label}"
                        }
                    }
                }
            }
        }
    }
}

/// How many suggestions the search box offers at once.
const SUGGESTIONS: usize = 8;

/// The packages and workers whose name contains `query`, names that start
/// with it first, then alphabetically; packages before workers on a tie.
pub(crate) fn suggest(query: &str, packages: &[String], workers: &[String]) -> Vec<EntityRef> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<(bool, String, u8, EntityRef)> = Vec::new();
    for (rank, names) in [(0u8, packages), (1u8, workers)] {
        for name in names {
            let lowered = name.to_lowercase();
            if !lowered.contains(&query) {
                continue;
            }
            let entity = if rank == 0 {
                PackageRef::from(name.as_str()).into()
            } else {
                WorkerRef::from(name.as_str()).into()
            };
            found.push((!lowered.starts_with(&query), lowered, rank, entity));
        }
    }
    found.sort_by(|a, b| (a.0, &a.1, a.2).cmp(&(b.0, &b.1, b.2)));
    found
        .into_iter()
        .take(SUGGESTIONS)
        .map(|(_, _, _, entity)| entity)
        .collect()
}

/// One search box over packages and workers: type part of a name, pick a
/// suggestion, and the log narrows to it.
///
/// Suggestions come from what the server tracks now; a deleted package's old
/// entries are reached from their rows' funnel instead.
#[component]
fn EntitySearch(about: Signal<Option<EntityRef>>) -> Element {
    let mut about = about;
    let mut typed = use_signal(String::new);
    let mut focused = use_signal(|| false);
    // Fetched once, for suggestions only: nothing breaks if they fail.
    let names = use_resource(move || async move {
        let Ok(client) = crate::api::client() else {
            return (Vec::new(), Vec::new());
        };
        let (packages, workers) = futures_util::future::join(
            client.list_packages(&aurcache_client::PackageQuery {
                dependencies: aurcache_client::DependencySelection::Include,
                ..Default::default()
            }),
            client.list_workers(),
        )
        .await;
        let mut packages: Vec<String> = packages
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.name)
            .collect();
        let mut workers: Vec<String> = workers
            .unwrap_or_default()
            .into_iter()
            .map(|w| w.name)
            .collect();
        packages.sort_unstable();
        packages.dedup();
        workers.sort_unstable();
        workers.dedup();
        (packages, workers)
    });
    let matches = match &*names.read_unchecked() {
        Some((packages, workers)) => suggest(&typed(), packages, workers),
        None => Vec::new(),
    };
    let first = matches.first().cloned();
    let mut choose = move |entity: EntityRef| {
        about.set(Some(entity));
        typed.set(String::new());
    };

    rsx! {
        div { class: "relative",
            input {
                class: "input input-bordered input-sm w-60",
                r#type: "search",
                placeholder: "Package or worker…",
                aria_label: "Search for a package or worker to filter by",
                autocomplete: "off",
                value: "{typed}",
                oninput: move |e| typed.set(e.value()),
                onfocus: move |_| focused.set(true),
                onblur: move |_| focused.set(false),
                onkeydown: move |e| {
                    if e.key() == Key::Enter
                        && let Some(entity) = first.clone()
                    {
                        choose(entity);
                    } else if e.key() == Key::Escape {
                        typed.set(String::new());
                    }
                },
            }
            if focused() && !matches.is_empty() {
                ul {
                    class: "menu menu-sm absolute z-20 mt-1 w-72 bg-base-100 rounded-box shadow",
                    role: "listbox",
                    for (key, label, what, entity) in matches.into_iter().map(|entity| {
                        (entity.to_string(), entity.label(), kind_of(&entity).to_lowercase(), entity)
                    }) {
                        li { key: "{key}",
                            // `mousedown`, not `click`: it lands before the
                            // input's blur hides the list.
                            button {
                                role: "option",
                                onmousedown: move |_| choose(entity.clone()),
                                span { class: "font-mono", "{label}" }
                                span { class: "badge badge-ghost badge-sm", "{what}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// What kind of thing a reference is, for the chip that names it.
fn kind_of(entity: &EntityRef) -> &'static str {
    match entity {
        EntityRef::Package(_) => "Package",
        EntityRef::Worker(_) => "Worker",
        EntityRef::Build(_) => "Build",
    }
}

/// Everything an entry is about, in the order its payload names it, with each
/// build's package after it: the choices its row menu offers.
///
/// Read from the payload rather than the decoded event, so an entry this build
/// cannot decode still offers them -- the same reading the server indexes by.
pub(crate) fn entities_of(entry: &LogEntry) -> Vec<EntityRef> {
    let mut found: Vec<EntityRef> = Vec::new();
    let mut push = |entity: EntityRef| {
        if !found.contains(&entity) {
            found.push(entity);
        }
    };
    let values = entry
        .data
        .as_object()
        .into_iter()
        .flat_map(|fields| fields.values())
        .flat_map(|value| match value {
            serde_json::Value::Array(items) => items.iter().collect::<Vec<_>>(),
            other => vec![other],
        });
    for value in values {
        let Some(entity) = value.as_str().and_then(|raw| raw.parse::<EntityRef>().ok()) else {
            continue;
        };
        let package = match &entity {
            EntityRef::Build(build) => Some(PackageRef::from(build.pkgbase.as_str()).into()),
            _ => None,
        };
        push(entity);
        if let Some(package) = package {
            push(package);
        }
    }
    if let Some(scope) = &entry.scope {
        push(scope.clone());
    }
    found
}

/// A row's filter menu: narrow the log to something the entry is about.
///
/// Shown when the row is hovered or the button focused, so a page of rows is
/// not a column of identical icons; always shown where there is no hover to
/// reveal it (a touch screen), and while its menu is open. Every row has one:
/// its kind is always something to narrow to.
#[component]
fn RowMenu(
    entry: LogEntry,
    on_about: EventHandler<EntityRef>,
    on_kind: EventHandler<String>,
) -> Element {
    let mut open = use_signal(|| false);
    let choices = entities_of(&entry);
    let kind = entry.kind.clone();
    let kind_name = kind_label(&entry.kind).to_string();
    let trigger = if open() {
        "btn btn-ghost btn-xs btn-square"
    } else {
        "btn btn-ghost btn-xs btn-square [@media(hover:hover)]:opacity-0 \
         [@media(hover:hover)]:group-hover:opacity-100 focus-visible:opacity-100"
    };
    rsx! {
        div { class: if open() { "dropdown dropdown-end dropdown-open" } else { "dropdown dropdown-end" },
            button {
                class: "{trigger}",
                title: "Narrow the log to what this entry is about",
                aria_label: "Narrow the log to what this entry is about",
                aria_haspopup: "menu",
                aria_expanded: "{open}",
                onclick: move |_| open.toggle(),
                crate::shell::FilterIcon {}
            }
            if open() {
                // Clicking anywhere else closes the menu: a transparent
                // backdrop catches the click, the same trick as a modal
                // backdrop. The menu itself sits above it.
                button {
                    class: "fixed inset-0 z-10 cursor-default",
                    aria_label: "Close menu",
                    tabindex: "-1",
                    onclick: move |_| open.set(false),
                }
                ul { class: "dropdown-content menu menu-sm bg-base-100 rounded-box shadow z-20 w-max max-w-[calc(100vw-2rem)] whitespace-nowrap",
                    role: "menu",
                    li {
                        button {
                            role: "menuitem",
                            onclick: move |_| {
                                open.set(false);
                                on_kind.call(kind.clone());
                            },
                            "Only entries like this: "
                            span { class: "font-medium", "{kind_name}" }
                        }
                    }
                    for (key, kind, label, entity) in choices.into_iter().map(|entity| {
                        (entity.to_string(), kind_of(&entity).to_lowercase(), entity.label(), entity)
                    }) {
                        li { key: "{key}",
                            button {
                                role: "menuitem",
                                onclick: move |_| {
                                    open.set(false);
                                    on_about.call(entity.clone());
                                },
                                "Only entries about this {kind}: "
                                span { class: "font-mono", "{label}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// How loud an entry is, for the eye rather than the filter.
///
/// Informational entries carry no badge at all: they are most of the log, and a
/// badge on every row would make the ones that matter harder to find, not
/// easier.
#[component]
fn SeverityBadge(severity: Severity) -> Element {
    let (label, class) = match severity {
        Severity::Info => return rsx! {},
        Severity::Warning => ("warning", "badge-warning"),
        Severity::Error => ("error", "badge-error"),
    };
    rsx! {
        span { class: "badge {class} badge-sm mr-2 align-middle", "{label}" }
    }
}

/// How many pages a log of this length has.
///
/// At least one, so an empty log reads as "page 1 of 1" rather than "of 0".
fn pages_of(total: u64) -> usize {
    (total as usize).div_ceil(PAGE_SIZE).max(1)
}

/// Columns that only appear once there is room for them.
const WIDE_ONLY: &str = "hidden md:table-cell";

#[component]
fn LogTable(
    entries: Vec<LogEntry>,
    on_about: EventHandler<EntityRef>,
    on_kind: EventHandler<String>,
) -> Element {
    rsx! {
        div { class: "overflow-x-auto",
            table { class: LIST_TABLE,
                thead {
                    tr {
                        // The two narrow columns take only what they need, so
                        // the text -- the column anyone is actually reading --
                        // gets the rest of the width.
                        th { class: "w-px whitespace-nowrap", "When" }
                        th { class: "{WIDE_ONLY} w-px whitespace-nowrap", "Who" }
                        th { "What" }
                        th { class: "w-px", span { class: "sr-only", "Options" } }
                    }
                }
                tbody {
                    for entry in entries.iter() {
                        tr { key: "{entry.id}", class: "group",
                            // On a phone the time drops under the date,
                            // halving the column the text has to share. The
                            // minimum keeps the date itself whole rather than
                            // broken at its hyphens.
                            td { class: "w-px md:whitespace-nowrap text-sm opacity-70",
                                div { class: "max-md:min-w-24",
                                    AbsoluteDate { ts: Some(entry.timestamp) }
                                }
                            }
                            td { class: "{WIDE_ONLY} w-px whitespace-nowrap text-sm",
                                {actor(entry.user.as_deref())}
                            }
                            // A word longer than a phone's column -- a git
                            // build's version, say -- breaks rather than
                            // widening the table past the screen.
                            td { class: "text-sm [overflow-wrap:anywhere]",
                                SeverityBadge { severity: entry.severity }
                                EntryText { entry: entry.clone() }
                            }
                            td { class: "w-px text-right",
                                RowMenu { entry: entry.clone(), on_about, on_kind }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Who to credit an entry to.
///
/// A missing user is the server acting on its own — a schedule firing, a
/// version check finding something — not an anonymous person. The Dart
/// frontend rendered that as "You", which is wrong in both directions: it
/// claims work you did not do, and on a multi-user instance it would claim
/// someone else's.
fn actor(user: Option<&str>) -> String {
    user.map_or_else(|| "AURCache".to_string(), ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::{LogTable, actor, entities_of, entity_route, suggest};
    use crate::listing::ViewParams;
    use crate::routes::Route;
    use aurcache_client::{BuildRef, EntityRef, Event, LogEntry, PackageRef, WorkerRef};
    use dioxus::prelude::*;

    fn entry(id: i32, event: &Event, user: Option<&str>) -> LogEntry {
        let wire = serde_json::to_value(event).unwrap();
        LogEntry {
            id,
            kind: event.kind().to_string(),
            severity: event.severity(),
            message: event.message(),
            data: wire["data"].clone(),
            scope: None,
            timestamp: 1_756_000_000 + i64::from(id),
            user: user.map(str::to_string),
        }
    }

    /// The table with a handler that goes nowhere: the handler is built
    /// inside a component, where the runtime it needs exists.
    #[component]
    fn Harness(entries: Vec<LogEntry>) -> Element {
        rsx! { LogTable { entries, on_about: move |_| {}, on_kind: move |_| {} } }
    }

    fn render(entries: Vec<LogEntry>) -> String {
        let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { entries });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn an_entry_with_no_user_is_credited_to_the_server() {
        assert_eq!(actor(Some("alice")), "alice");
        assert_eq!(actor(None), "AURCache");
    }

    /// The three columns are the whole screen, so a row that drops one is the
    /// only defect worth guarding here.
    ///
    /// Only events that name nothing: a `Link` needs a router context that a
    /// bare render test has none of. That the names link is asserted by the
    /// browser suite, and where they go by `entity_route` below.
    #[test]
    fn a_row_shows_when_who_and_what() {
        let html = render(vec![
            entry(
                1,
                &Event::ServerStarted {
                    version: "1.2.3".to_string(),
                },
                Some("alice"),
            ),
            entry(
                2,
                &Event::UpdateQueueFailed {
                    error: "disk full".to_string(),
                },
                None,
            ),
        ]);

        assert!(html.contains("AURCache 1.2.3 started"), "{html}");
        assert!(html.contains("alice"), "{html}");
        assert!(html.contains("disk full"), "{html}");
        assert!(html.contains("AURCache"), "unattributed entry: {html}");

        // A failure is marked; ordinary news is not, or the badge would be on
        // every row and pick nothing out.
        assert!(html.contains("badge-error"), "{html}");
        assert!(!html.contains("badge-info"), "{html}");
    }

    /// An entry this build cannot decode -- a kind from a newer server, or a
    /// payload that no longer fits -- still reads, from the sentence stored
    /// with it.
    #[test]
    fn an_unknown_entry_shows_its_stored_sentence() {
        let mut from_the_future = entry(
            1,
            &Event::ServerStarted {
                version: "9".to_string(),
            },
            None,
        );
        from_the_future.kind = "time.travelled".to_string();
        from_the_future.message = "someone arrived from next year".to_string();
        let mut stale = entry(
            2,
            &Event::ServerStarted {
                version: "9".to_string(),
            },
            None,
        );
        stale.data = serde_json::json!({"release": "9"});
        stale.message = "an older shape of a start".to_string();

        let html = render(vec![from_the_future, stale]);
        assert!(html.contains("someone arrived from next year"), "{html}");
        assert!(html.contains("an older shape of a start"), "{html}");
    }

    /// A row's menu offers everything the entry names, a build's package with
    /// it, each once -- and reads the payload, so an entry this build cannot
    /// decode still offers them.
    #[test]
    fn a_row_offers_everything_it_names() {
        let build = || BuildRef {
            pkgbase: "hello".to_string(),
            number: 7,
        };
        let mut reaped = entry(
            1,
            &Event::WorkerReaped {
                workers: vec![WorkerRef::from("builder-01")],
                retried: vec![build()],
                failed: vec![build()],
            },
            None,
        );
        reaped.kind = "from.the.future".to_string();
        let offered = entities_of(&reaped);
        assert_eq!(offered.len(), 3, "{offered:?}");
        assert!(offered.contains(&EntityRef::Build(build())));
        assert!(offered.contains(&EntityRef::Package(PackageRef::from("hello"))));
        assert!(offered.contains(&EntityRef::Worker(WorkerRef::from("builder-01"))));

        let quiet = entry(
            2,
            &Event::ServerStarted {
                version: "1".to_string(),
            },
            None,
        );
        assert!(entities_of(&quiet).is_empty(), "nothing to offer, no menu");
    }

    /// Names that start with what was typed come first, then any containing
    /// it; packages and workers alike, each marked for what it is.
    #[test]
    fn suggestions_put_prefixes_first_and_mix_packages_and_workers() {
        let packages = vec![
            "hello".to_string(),
            "othello".to_string(),
            "yay".to_string(),
        ];
        let workers = vec!["hel-builder".to_string(), "builder-01".to_string()];
        assert_eq!(
            suggest("HEL", &packages, &workers),
            vec![
                EntityRef::Worker(WorkerRef::from("hel-builder")),
                EntityRef::Package(PackageRef::from("hello")),
                EntityRef::Package(PackageRef::from("othello")),
            ]
        );
        assert!(suggest("  ", &packages, &workers).is_empty());
        assert!(suggest("nothing", &packages, &workers).is_empty());
    }

    /// Where each kind of reference opens. A worker goes by name, which is how
    /// its page is addressed and which copes with two machines sharing one.
    #[test]
    fn a_reference_opens_its_own_page() {
        assert_eq!(
            entity_route(&EntityRef::Package(PackageRef::from("hello"))),
            Route::Package {
                pkgbase: "hello".to_string()
            }
        );
        assert_eq!(
            entity_route(&EntityRef::Worker(WorkerRef::from("ci/runner"))),
            Route::Worker {
                name: vec!["ci".to_string(), "runner".to_string()]
            }
        );
        assert_eq!(
            entity_route(&EntityRef::Build(BuildRef {
                pkgbase: "hello".to_string(),
                number: 7,
            })),
            Route::Build {
                pkgbase: "hello".to_string(),
                number: 7
            }
        );
    }

    /// The Activity card's title links to the log narrowed to this page's
    /// subject. The mapping SSR cannot render (links need a router) but the
    /// browser suite navigates.
    #[test]
    fn the_activity_title_links_to_the_filtered_log() {
        let pkg = Route::Logs {
            view: ViewParams::about(PackageRef::from("hello")),
        }
        .to_string();
        assert!(pkg.contains("e=pkg:hello"), "{pkg}");

        let worker = Route::Logs {
            view: ViewParams::about(WorkerRef::from("builder-01")),
        }
        .to_string();
        assert!(worker.contains("e=worker:builder-01"), "{worker}");
    }
}
