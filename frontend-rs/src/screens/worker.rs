//! One worker: what it is, what it has done, and what it is configured with.
//!
//! Split from the fleet list because the settings are the bulk of it -- a
//! machine declares a couple of dozen, each with a description worth reading,
//! which is a page rather than a column. The list answers "is the fleet
//! healthy"; this answers "what is this machine actually running".
//!
//! Settings are set from here and delivered to the worker on its next
//! heartbeat. The page says where each running value came from -- including a
//! pin on the machine that outranks what is set here -- and which values the
//! worker refused. See `design/implemented/worker-configuration.md`.

use crate::dates::RelativeDate;
use crate::format::now_secs;
use crate::routes::Route;
use crate::screens::workers::{
    KindBadge, Liveness, PausedBadge, StatusBadge, WorkerAction, architectures,
};
use aurcache_client::{Worker as WorkerRow, WorkerConfigUpdate, WorkerConfigView};
use aurcache_common::worker_config::{
    Applies, EffectiveConfig, EffectiveSetting, EffectiveSource, SettingDecl, SettingStatus,
    ValueKind,
};
use dioxus::prelude::*;
use std::collections::BTreeMap;

/// How many characters of a fingerprint disambiguate two workers of one name.
///
/// The git short-hash idiom, and for the same reason: long enough that a
/// collision needs contriving, short enough to read in a URL. The page shows
/// the fingerprint in full.
const SHORT_FINGERPRINT: usize = 12;

/// The head of a fingerprint, as a URL carries it.
#[must_use]
pub(crate) fn short_fingerprint(fingerprint: &str) -> String {
    fingerprint.chars().take(SHORT_FINGERPRINT).collect()
}

/// What a URL naming a worker turned out to mean.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolved<'a> {
    /// No worker answers to it.
    None,
    /// Exactly one does.
    One(&'a WorkerRow),
    /// Several do, and the URL does not say which.
    ///
    /// Not a case to guess at: the usual way to get here is a machine replaced
    /// by another of the same hostname, so the rows differ in exactly the thing
    /// the reader cares about -- one is retired and one is live.
    Several(Vec<&'a WorkerRow>),
}

/// Find the worker a URL points at.
///
/// A fingerprint identifies one outright. A name may not: a worker is called
/// whatever its machine calls itself, and a retired row keeps its name for
/// ever, so this hands back the ambiguity rather than resolving it by picking.
pub(crate) fn resolve<'a>(workers: &'a [WorkerRow], name: &str, fingerprint: &str) -> Resolved<'a> {
    let matches: Vec<&WorkerRow> = if fingerprint.is_empty() {
        workers.iter().filter(|w| w.name == name).collect()
    } else {
        // Hex, and a URL is not a place to be particular about its case.
        let prefix = fingerprint.to_ascii_lowercase();
        workers
            .iter()
            .filter(|w| w.cert_fingerprint.to_ascii_lowercase().starts_with(&prefix))
            .collect()
    };
    match matches.len() {
        0 => Resolved::None,
        1 => Resolved::One(matches[0]),
        _ => Resolved::Several(matches),
    }
}

/// A worker name as the URL carries it.
///
/// Split on `/` because the route segment is a catch-all: a name is free text,
/// and one containing a slash has to survive the trip rather than being read
/// back as something else.
#[must_use]
pub(crate) fn name_segments(name: &str) -> Vec<String> {
    name.split('/').map(str::to_string).collect()
}

/// Names claimed by more than one worker: a link to one of those goes by
/// fingerprint, never by name.
///
/// Counted once per table render rather than once per row — the fleet only
/// grows, and asking the whole fleet per row is quadratic.
pub(crate) fn shared_names(fleet: &[WorkerRow]) -> std::collections::HashSet<&str> {
    let mut seen = std::collections::HashSet::new();
    let mut shared = std::collections::HashSet::new();
    for worker in fleet {
        if !seen.insert(worker.name.as_str()) {
            shared.insert(worker.name.as_str());
        }
    }
    shared
}

/// The URL for a worker, among the fleet it belongs to.
///
/// The bare name wherever it is unambiguous, which is nearly always. A worker
/// sharing its name with another is linked by fingerprint instead, so a link
/// from the list never lands on a chooser -- that is for a name someone typed
/// or pasted.
///
/// `shared` comes from [`shared_names`]: the table counts once per render and
/// hands each row its answer.
#[must_use]
pub(crate) fn worker_route(worker: &WorkerRow, shared: bool) -> Route {
    if shared {
        Route::WorkerByFingerprint {
            fingerprint: short_fingerprint(&worker.cert_fingerprint),
        }
    } else {
        Route::Worker {
            name: name_segments(&worker.name),
        }
    }
}

/// One worker, by the name it calls itself.
#[component]
pub fn Worker(name: Vec<String>) -> Element {
    rsx! {
        WorkerPage { name: name.join("/"), fingerprint: String::new() }
    }
}

/// One worker, by the fingerprint that is its actual identity.
#[component]
pub fn WorkerByFingerprint(fingerprint: String) -> Element {
    rsx! {
        WorkerPage { name: String::new(), fingerprint }
    }
}

/// The page itself, once the URL has been turned into a worker.
///
/// Resolved against the fleet list rather than an endpoint of its own:
/// everything the header shows is already in it, including the liveness and the
/// build record the server derives, and a second shape for one row would be a
/// second thing to keep in step.
#[component]
fn WorkerPage(name: String, fingerprint: String) -> Element {
    let mut workers = use_resource(move || async move {
        crate::api::client()?
            .list_workers()
            .await
            .map_err(|e| e.to_string())
    });

    // No such worker: back to the fleet, saying which one was asked for. A
    // name several workers share still offers the choice below.
    let notice = crate::notice::use_notice();
    use_effect(use_reactive(
        &(name.clone(), fingerprint.clone()),
        move |(name, fingerprint)| {
            let Some(Ok(list)) = &*workers.read() else {
                return;
            };
            if matches!(resolve(list, &name, &fingerprint), Resolved::None) {
                let text = if name.is_empty() {
                    format!("No worker has the certificate {fingerprint}.")
                } else {
                    format!("No worker is called {name}.")
                };
                crate::notice::redirect(
                    notice,
                    Route::Workers {},
                    crate::notice::Level::Error,
                    text,
                );
            }
        },
    ));

    // Pausing and resuming from the page, as from the fleet list; the header
    // shows what the server made of it once the list is read again.
    let mut busy = use_signal(|| false);
    let mut outcome = use_signal(|| Option::<Result<String, String>>::None);
    let act = move |(id, action): (i32, WorkerAction)| async move {
        busy.set(true);
        let result = action.perform(id).await;
        busy.set(false);
        match result {
            Ok(()) => {
                outcome.set(Some(Ok(action.done().to_string())));
                workers.restart();
            }
            Err(e) => outcome.set(Some(Err(e))),
        }
    };

    rsx! {
        div { class: "flex flex-col gap-4",
            if let Some(result) = outcome() {
                div {
                    class: if result.is_ok() { "alert alert-success alert-soft text-sm" } else { "alert alert-error alert-soft text-sm" },
                    span { {result.as_ref().unwrap_or_else(|error| error).clone()} }
                }
            }
            match &*workers.read_unchecked() {
                None => rsx! {
                    span { class: "loading loading-spinner loading-md" }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error alert-soft", "{e}" }
                },
                Some(Ok(list)) => match resolve(list, &name, &fingerprint) {
                    // A revoked worker keeps its row, so this is a name that
                    // never enrolled -- or one whose row was removed outright.
                    // On its way back to the fleet; see the redirect above.
                    Resolved::None => rsx! {
                        span { class: "loading loading-spinner loading-md" }
                    },
                    Resolved::One(worker) => rsx! {
                        WorkerHeader { worker: worker.clone(), busy: busy(), act }
                        div { class: "card bg-base-100 shadow-xl",
                            div { class: "card-body",
                                h2 { class: "card-title text-base", "Settings" }
                                p { class: "text-sm opacity-70",
                                    "What this worker accepts, and what each setting resolved to on that machine. "
                                    "A value set here reaches the worker on its next heartbeat, unless the machine "
                                    "pins that setting in its own environment."
                                }
                                WorkerConfig { id: worker.id }
                            }
                        }
                        crate::screens::logs::RecentActivity {
                            about: aurcache_client::WorkerRef::from(worker.name.as_str()).into(),
                        }
                    },
                    Resolved::Several(matches) => rsx! {
                        WorkerChooser { name: name.clone(), matches: matches.into_iter().cloned().collect::<Vec<_>>() }
                    },
                },
            }
        }
    }
}

/// Which of the machines called this did you mean?
///
/// Shows what tells them apart -- whether each is still in the fleet, when it
/// last called in, and the fingerprint that is its actual identity.
#[component]
fn WorkerChooser(name: String, matches: Vec<WorkerRow>) -> Element {
    let now = now_secs();
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h1 { class: "card-title",
                    span { class: "font-mono", "{name}" }
                }
                p { class: "text-sm opacity-70",
                    "{matches.len()} workers call themselves this. A name is whatever a machine "
                    "reports; the fingerprint below is the identity behind it."
                }
                ul { class: "flex flex-col gap-2 mt-2",
                    for worker in matches.iter() {
                        li { key: "{worker.cert_fingerprint}",
                            Link {
                                class: "flex flex-wrap items-center gap-2 p-2 rounded hover:bg-base-200",
                                to: Route::WorkerByFingerprint {
                                    fingerprint: short_fingerprint(&worker.cert_fingerprint),
                                },
                                span { class: "font-mono text-sm", "{short_fingerprint(&worker.cert_fingerprint)}" }
                                StatusBadge { status: worker.status }
                                Liveness { worker: worker.clone() }
                                span { class: "text-sm opacity-60",
                                    if worker.last_seen.is_some() {
                                        RelativeDate { ts: worker.last_seen, now }
                                    } else {
                                        "never seen"
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

/// Who this machine is, and how it is doing.
///
/// The same facts the list column shows, because they are the ones an operator
/// arrives having just read -- the page should confirm what they clicked, not
/// restate it differently.
#[component]
fn WorkerHeader(
    worker: WorkerRow,
    /// An action on this worker is in flight.
    busy: bool,
    act: EventHandler<(i32, WorkerAction)>,
) -> Element {
    let now = now_secs();
    let id = worker.id;
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                div { class: "flex items-center min-h-8",
                    h1 { class: "card-title block leading-8 break-all",
                        Link {
                            class: "opacity-60 link-hover",
                            to: Route::Workers {},
                            "Workers"
                        }
                        span { class: "opacity-30 mx-2", "/" }
                        span { class: "font-mono", "{worker.name}" }
                    }
                }
                div { class: "flex flex-wrap items-center gap-2",
                    StatusBadge { status: worker.status }
                    if worker.paused {
                        PausedBadge {}
                    }
                    KindBadge { worker: worker.clone() }
                    Liveness { worker: worker.clone() }
                    // No "Log" link here: the Activity card below already
                    // links to the log narrowed to this worker.
                    // Only an approved worker claims anything to stop claiming.
                    if worker.status.can_build() {
                        div { class: "ml-auto",
                            if worker.paused {
                                button {
                                    class: "btn btn-sm btn-primary",
                                    disabled: busy,
                                    title: WorkerAction::Resume.hint(),
                                    onclick: move |_| act.call((id, WorkerAction::Resume)),
                                    "Resume intake"
                                }
                            } else {
                                button {
                                    class: "btn btn-sm btn-outline",
                                    disabled: busy,
                                    title: WorkerAction::Pause.hint(),
                                    onclick: move |_| act.call((id, WorkerAction::Pause)),
                                    "Stop intake"
                                }
                            }
                        }
                    }
                }
                if worker.paused {
                    p { class: "text-sm opacity-70",
                        if worker.active_builds > 0 {
                            "Intake stopped: new builds go to other workers. "
                            "{worker.active_builds} still running here; it is empty once they finish."
                        } else {
                            "Intake stopped, and idle: nothing running here. Safe to reboot, upgrade "
                            "or retire; resume intake to put it back to work."
                        }
                    }
                }
                dl { class: "grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm mt-2",
                    dt { class: "opacity-60", "Architectures" }
                    dd { {architectures(&worker)} }

                    dt { class: "opacity-60", "Reserved for" }
                    dd {
                        if worker.package_affinity.is_empty() {
                            span { class: "opacity-40", "—" }
                        } else {
                            div { class: "flex flex-wrap gap-1",
                                for package in worker.package_affinity.iter() {
                                    Link {
                                        key: "{package}",
                                        class: "badge badge-outline badge-sm font-mono link-hover",
                                        to: Route::Package { pkgbase: package.clone() },
                                        "{package}"
                                    }
                                }
                            }
                        }
                    }

                    dt { class: "opacity-60", "Priority" }
                    dd {
                        if worker.priority == 0 {
                            span { class: "opacity-40", title: "No preference", "—" }
                        } else {
                            "{worker.priority}"
                        }
                    }

                    dt { class: "opacity-60", "Builds" }
                    dd { "{worker.active_builds} running · {worker.successful_builds} succeeded · {worker.failed_builds} failed" }

                    dt { class: "opacity-60", "Last seen" }
                    dd {
                        if worker.last_seen.is_some() {
                            RelativeDate { ts: worker.last_seen, now }
                        } else {
                            span { class: "opacity-60", "never" }
                        }
                    }

                    // The identity behind the name, which is not unique. Last
                    // because it is the one nobody reads until they have to.
                    dt { class: "opacity-60", "Fingerprint" }
                    dd { class: "font-mono break-all opacity-70", "{worker.cert_fingerprint}" }
                }
            }
        }
    }
}

/// Edits waiting to be saved, by key: `Some` sets a value, `None` removes one.
type Drafts = BTreeMap<String, Option<String>>;

/// What a field's new text means as a change, given what is set here now.
///
/// `None` is "no change" -- the text says what is already stored, or the field
/// was emptied where nothing was stored. Emptying a field that held a value is
/// removing it, so the worker falls back to its own.
#[must_use]
pub(crate) fn stage(stored: Option<&str>, typed: &str) -> Option<Option<String>> {
    let typed = typed.trim();
    match (stored, typed.is_empty()) {
        (Some(_), true) => Some(None),
        (None, true) => None,
        (Some(stored), false) if stored == typed => None,
        _ => Some(Some(typed.to_string())),
    }
}

/// Whether the worker has the values last saved here.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// It is running the latest save, or nothing has been saved for it.
    Current,
    /// It has not taken the latest save yet.
    Pending,
}

#[must_use]
pub(crate) fn delivery(view: &WorkerConfigView) -> Delivery {
    let received = view
        .effective
        .as_ref()
        .and_then(|e| e.received_revision.as_deref());
    match received {
        Some(held) if held == view.revision => Delivery::Current,
        // Nothing to take is nothing missing.
        None if view.values.is_empty() => Delivery::Current,
        _ => Delivery::Pending,
    }
}

/// When a change to a setting takes hold, as the row says it.
#[must_use]
pub(crate) fn applies_note(applies: Applies) -> &'static str {
    match applies {
        Applies::NextJob => "from the next build; running builds keep theirs",
        Applies::NextLoop => "from its next pass",
        Applies::Immediately => "at once, including for builds already running",
    }
}

/// A worker's declared settings: what each resolved to, and a field to set it
/// from here.
///
/// Fetched apart from the workers list: that list is polled while anything is
/// building, and a declaration is kilobytes of descriptions that change only
/// when a worker is upgraded.
///
/// Edits are staged and saved together, because related settings are changed
/// together -- fewer builds with more memory each -- and the server writes a
/// save in one transaction so the worker never runs half of one.
#[component]
fn WorkerConfig(id: i32) -> Element {
    let mut config = use_resource(move || async move {
        crate::api::client()?
            .worker_config(id)
            .await
            .map_err(|e| e.to_string())
    });
    let mut drafts = use_signal(Drafts::new);
    let mut status = use_signal(|| Option::<Result<String, String>>::None);
    let mut saving = use_signal(|| false);

    // Refreshed briskly while a save is on its way to the worker, so the page
    // shows it landing -- or being refused -- without a reload.
    let waiting = matches!(
        &*config.read(),
        Some(Ok(view)) if delivery(view) != Delivery::Current
    );
    crate::poll::use_poll(config, waiting);

    let save = move |_| async move {
        let update = WorkerConfigUpdate { settings: drafts() };
        saving.set(true);
        let result = match crate::api::client() {
            Ok(client) => client
                .update_worker_config(id, &update)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        saving.set(false);
        match result {
            Ok(_) => {
                drafts.set(Drafts::new());
                status.set(Some(Ok(
                    "Saved. The worker picks it up on its next heartbeat.".to_string(),
                )));
                config.restart();
            }
            Err(e) => status.set(Some(Err(format!("Could not save: {e}")))),
        }
    };

    let pending = drafts.read().len();
    rsx! {
        div {
            if let Some(result) = status() {
                div {
                    class: if result.is_ok() { "alert alert-success alert-soft text-sm mb-3" } else { "alert alert-error alert-soft text-sm mb-3" },
                    span { {result.as_ref().unwrap_or_else(|error| error).clone()} }
                }
            }
            match &*config.read_unchecked() {
                None => rsx! {
                    span { class: "loading loading-spinner loading-sm" }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error alert-soft text-sm", "{e}" }
                },
                Some(Ok(view)) => match &view.settings {
                    declared if declared.is_empty() => rsx! {
                        div { class: "text-sm opacity-70", "This worker declares no settings." }
                    },
                    declared => rsx! {
                        DeliveryNote { view: view.clone() }
                        SettingsTable {
                            declared: declared.clone(),
                            effective: view.effective.clone(),
                            values: view.values.clone(),
                            drafts,
                        }
                        Retired { declared: declared.clone(), values: view.values.clone(), drafts }
                    },
                },
            }
            if pending > 0 {
                div { class: "flex items-center gap-2 mt-4 sticky bottom-0 bg-base-100 py-2",
                    button {
                        class: "btn btn-primary btn-sm",
                        disabled: saving(),
                        onclick: save,
                        if pending == 1 { "Save 1 change" } else { "Save {pending} changes" }
                    }
                    button {
                        class: "btn btn-ghost btn-sm",
                        disabled: saving(),
                        onclick: move |_| drafts.set(Drafts::new()),
                        "Discard"
                    }
                }
            }
        }
    }
}

/// Whether the worker has what was last saved here, when it matters.
#[component]
fn DeliveryNote(view: WorkerConfigView) -> Element {
    match delivery(&view) {
        Delivery::Current => rsx! {},
        Delivery::Pending => rsx! {
            div { class: "alert alert-info alert-soft text-sm mb-3",
                "The worker has not picked up the latest save yet. It does on its next "
                "heartbeat, or when it next checks in if it is offline."
            }
        },
    }
}

/// The declared settings, grouped the way the worker grouped them.
#[component]
fn SettingsTable(
    declared: Vec<SettingDecl>,
    effective: Option<EffectiveConfig>,
    values: BTreeMap<String, String>,
    drafts: Signal<Drafts>,
) -> Element {
    let groups = group_by_category(&declared);
    rsx! {
        if effective.is_none() {
            div { class: "text-sm opacity-70 mb-3",
                "Waiting for this worker's first report; these are the settings it accepts, not yet the values it is running."
            }
        }
        div { class: "flex flex-col gap-4",
            for Category { name: category, settings } in groups {
                div { key: "{category}",
                    div { class: "text-xs uppercase tracking-wide opacity-60 mb-1", "{category}" }
                    div { class: "overflow-x-auto",
                        table { class: "table table-sm",
                            tbody {
                                for decl in settings {
                                    SettingRow {
                                        key: "{decl.key}",
                                        decl: decl.clone(),
                                        effective: effective
                                            .as_ref()
                                            .and_then(|e| e.settings.get(&decl.key).cloned()),
                                        stored: values.get(&decl.key).cloned(),
                                        drafts,
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

/// Values set here for keys the worker no longer declares -- renamed or
/// dropped by an upgrade. Kept rather than deleted behind the operator's back,
/// and listed so they can be cleared.
#[component]
fn Retired(
    declared: Vec<SettingDecl>,
    values: BTreeMap<String, String>,
    mut drafts: Signal<Drafts>,
) -> Element {
    let retired: Vec<(String, String)> = values
        .into_iter()
        .filter(|(key, _)| !declared.iter().any(|decl| &decl.key == key))
        .collect();
    if retired.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "mt-4",
            div { class: "text-xs uppercase tracking-wide opacity-60 mb-1", "No longer offered by this worker" }
            ul { class: "flex flex-col gap-1",
                for (key, value) in retired {
                    li { key: "{key}", class: "flex items-center gap-2 text-sm",
                        span { class: "font-mono", "{key} = {value}" }
                        if drafts.read().get(&key) == Some(&None) {
                            span { class: "badge badge-ghost badge-sm", "to be removed" }
                        } else {
                            button {
                                class: "btn btn-ghost btn-xs",
                                onclick: move |_| {
                                    drafts.write().insert(key.clone(), None);
                                },
                                "Remove"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Declared settings by category, each category in the order the worker first
/// mentioned it.
///
/// Grouped rather than sorted: the worker's order puts related settings
/// together, and a category can be declared in two tables (the protocol's and
/// the executor's) without showing up twice.
fn group_by_category(declared: &[SettingDecl]) -> Vec<Category<'_>> {
    let mut groups: Vec<Category<'_>> = Vec::new();
    for decl in declared {
        match groups.iter_mut().find(|group| group.name == decl.category) {
            Some(group) => group.settings.push(decl),
            None => groups.push(Category {
                name: decl.category.clone(),
                settings: vec![decl],
            }),
        }
    }
    groups
}

/// The declared settings of one category; see [`group_by_category`].
struct Category<'a> {
    name: String,
    settings: Vec<&'a SettingDecl>,
}

/// One setting: a field holding the value set here, what the worker is
/// running, and where that came from.
#[component]
fn SettingRow(
    decl: SettingDecl,
    effective: Option<EffectiveSetting>,
    stored: Option<String>,
    mut drafts: Signal<Drafts>,
) -> Element {
    let key = decl.key.clone();
    let draft = drafts.read().get(&key).cloned();
    let dirty = draft.is_some();
    // A pin on the machine wins over anything saved here, so a field would
    // accept a change that has no effect. The note below says how to hand the
    // setting over instead.
    let pinned = effective
        .as_ref()
        .is_some_and(|e| e.source == EffectiveSource::Env);
    let running = effective.as_ref().and_then(|e| e.value.clone());
    let shown = match &draft {
        Some(Some(value)) => value.clone(),
        Some(None) => String::new(),
        None if pinned => running.clone().unwrap_or_default(),
        None => stored.clone().unwrap_or_default(),
    };
    // With nothing set here, the field is empty and says what the worker runs
    // without it -- the value an empty field means.
    let placeholder = running
        .or_else(|| decl.default.clone())
        .unwrap_or_else(|| "unset".to_string());
    let mut on_edit = {
        let key = key.clone();
        let stored = stored.clone();
        move |typed: String| match stage(stored.as_deref(), &typed) {
            Some(change) => {
                drafts.write().insert(key.clone(), change);
            }
            None => {
                drafts.write().remove(&key);
            }
        }
    };
    let notice = effective.as_ref().and_then(|e| match e.status {
        SettingStatus::Rejected | SettingStatus::Overridden => e.reason.clone(),
        SettingStatus::Applied | SettingStatus::Unsupported => None,
    });
    let warn = matches!(
        effective.as_ref().map(|e| e.status),
        Some(SettingStatus::Rejected)
    );

    rsx! {
        tr {
            td { class: "align-top w-1/3",
                div { class: "font-mono text-sm", "{decl.key}" }
                div { class: "text-xs opacity-60", "{decl.description}" }
            }
            td { class: "align-top",
                div { class: "flex items-center gap-2",
                    {
                        match &decl.kind {
                            ValueKind::Bool => rsx! {
                                select {
                                    class: "select select-bordered select-sm font-mono",
                                    name: "{decl.key}",
                                    disabled: pinned,
                                    value: "{shown}",
                                    onchange: move |e| on_edit(e.value()),
                                    option { value: "", "worker's own ({placeholder})" }
                                    option { value: "true", "true" }
                                    option { value: "false", "false" }
                                }
                            },
                            ValueKind::Choice { options } => rsx! {
                                select {
                                    class: "select select-bordered select-sm font-mono",
                                    name: "{decl.key}",
                                    disabled: pinned,
                                    value: "{shown}",
                                    onchange: move |e| on_edit(e.value()),
                                    option { value: "", "worker's own ({placeholder})" }
                                    for choice in options.iter() {
                                        option { key: "{choice}", value: "{choice}", "{choice}" }
                                    }
                                }
                            },
                            ValueKind::Integer { .. } | ValueKind::Float { .. } => rsx! {
                                input {
                                    r#type: "number",
                                    step: if matches!(decl.kind, ValueKind::Float { .. }) { "any" } else { "1" },
                                    class: "input input-bordered input-sm w-40 font-mono",
                                    name: "{decl.key}",
                                    disabled: pinned,
                                    placeholder: "{placeholder}",
                                    value: "{shown}",
                                    oninput: move |e| on_edit(e.value()),
                                }
                            },
                            ValueKind::Size | ValueKind::Duration | ValueKind::Text
                            | ValueKind::List | ValueKind::Unknown => rsx! {
                                input {
                                    r#type: "text",
                                    class: "input input-bordered input-sm w-48 font-mono text-xs",
                                    name: "{decl.key}",
                                    disabled: pinned,
                                    placeholder: "{placeholder}",
                                    value: "{shown}",
                                    oninput: move |e| on_edit(e.value()),
                                }
                            },
                        }
                    }
                    if stored.is_some() && !pinned && draft != Some(None) {
                        button {
                            class: "btn btn-ghost btn-xs",
                            title: "Remove the value set here, so the worker falls back to its own",
                            onclick: move |_| {
                                drafts.write().insert(key.clone(), None);
                            },
                            "Reset"
                        }
                    }
                }
                if dirty {
                    div { class: "text-xs opacity-60 mt-1",
                        match &draft {
                            Some(None) => rsx! { "Removed on save: the worker goes back to its own value, " },
                            _ => rsx! { "Takes effect " },
                        }
                        "{applies_note(decl.applies)}."
                    }
                }
            }
            td { class: "align-top text-xs w-64 text-right",
                if let Some(effective) = effective.as_ref() {
                    SourceNote { decl, effective: effective.clone() }
                } else {
                    span { class: "opacity-40", "not reported" }
                }
            }
        }
        if let Some(reason) = notice {
            tr {
                td { colspan: 3, class: "pt-0",
                    div {
                        class: if warn { "alert alert-warning alert-soft text-xs py-1" } else { "alert alert-soft text-xs py-1" },
                        "{reason}"
                    }
                }
            }
        }
    }
}

/// Where a value came from, in the operator's own vocabulary.
///
/// Names the variable rather than the concept: what someone needs in order to
/// change a value is the name they would edit on that machine, and "pinned by
/// the environment" does not tell them which line to look at.
#[component]
fn SourceNote(decl: SettingDecl, effective: EffectiveSetting) -> Element {
    let var = decl.env_var.clone().unwrap_or_default();
    let (label, class) = match effective.source {
        EffectiveSource::Env => (format!("pinned by {var}"), "badge-warning"),
        EffectiveSource::EnvDefault => (format!("{var}_DEFAULT"), "badge-ghost"),
        EffectiveSource::Server => ("set here".to_string(), "badge-info"),
        EffectiveSource::Default => ("built-in default".to_string(), "badge-ghost"),
    };
    rsx! {
        div { class: "flex flex-col gap-1 items-end text-right",
            span { class: "badge {class} badge-sm font-mono whitespace-nowrap", "{label}" }
            // Only worth saying where it differs from what is running: repeating
            // the value as its own fallback is noise on most rows.
            if let Some(fallback) = decl.default.as_ref()
                && effective.value.as_ref() != Some(fallback)
            {
                span { class: "opacity-60 whitespace-nowrap", "falls back to {fallback}" }
            }
            if effective.status == SettingStatus::Overridden {
                span { class: "opacity-60", "a value is set here but not in effect" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Resolved, group_by_category, name_segments, resolve, shared_names, short_fingerprint,
        worker_route,
    };
    use crate::routes::Route;
    use aurcache_client::{ApprovalStatus, Worker as WorkerRow};
    use aurcache_common::worker_config::{Applies, SettingDecl, ValueKind};

    fn worker(id: i32, name: &str, fingerprint: &str, status: ApprovalStatus) -> WorkerRow {
        WorkerRow {
            id,
            name: name.to_string(),
            status,
            cert_fingerprint: fingerprint.to_string(),
            native_arches: vec!["x86_64".to_string()],
            emulated_arches: Vec::new(),
            package_affinity: Vec::new(),
            priority: 0,
            last_seen: None,
            version: None,
            kind: None,
            online: false,
            active_builds: 0,
            successful_builds: 0,
            failed_builds: 0,
            settings_rejected: None,
            paused: false,
        }
    }

    #[test]
    fn a_name_nothing_answers_to_resolves_to_nothing() {
        let fleet = [worker(
            1,
            "builder-01",
            "aaaa1111",
            ApprovalStatus::Approved,
        )];
        assert_eq!(resolve(&fleet, "builder-02", ""), Resolved::None);
    }

    #[test]
    fn a_unique_name_is_enough() {
        let fleet = [
            worker(1, "builder-01", "aaaa1111", ApprovalStatus::Approved),
            worker(2, "builder-arm", "bbbb2222", ApprovalStatus::Approved),
        ];
        assert_eq!(resolve(&fleet, "builder-01", ""), Resolved::One(&fleet[0]));
    }

    /// The case that rules out a unique-name constraint: a machine is replaced
    /// by another with the same hostname, and the retired row keeps its name.
    /// Neither is the obvious answer, so the URL has to say which.
    #[test]
    fn a_shared_name_resolves_to_all_of_them() {
        let fleet = [
            worker(1, "builder-01", "aaaa1111", ApprovalStatus::Revoked),
            worker(2, "builder-01", "bbbb2222", ApprovalStatus::Approved),
        ];
        assert_eq!(
            resolve(&fleet, "builder-01", ""),
            Resolved::Several(vec![&fleet[0], &fleet[1]])
        );
        // The fingerprint picks one out on its own, case-insensitively.
        assert_eq!(resolve(&fleet, "", "bbbb"), Resolved::One(&fleet[1]));
        assert_eq!(resolve(&fleet, "", "BBBB"), Resolved::One(&fleet[1]));
    }

    /// A fingerprint nothing starts with resolves to nothing, rather than
    /// widening back to every worker.
    #[test]
    fn a_fingerprint_matching_none_of_them_resolves_to_nothing() {
        let fleet = [
            worker(1, "builder-01", "aaaa1111", ApprovalStatus::Approved),
            worker(2, "builder-01", "bbbb2222", ApprovalStatus::Approved),
        ];
        assert_eq!(resolve(&fleet, "", "cccc"), Resolved::None);
    }

    /// Links from the list never land on the chooser: the bare name where it is
    /// unambiguous, the fingerprint too where it is not.
    #[test]
    fn a_link_is_bare_until_the_name_is_shared() {
        let unique = [worker(
            1,
            "builder-01",
            "aaaa1111",
            ApprovalStatus::Approved,
        )];
        let shared = shared_names(&unique);
        assert!(shared.is_empty());
        assert_eq!(
            worker_route(&unique[0], shared.contains("builder-01")),
            Route::Worker {
                name: vec!["builder-01".to_string()]
            }
        );

        let fleet = [
            worker(1, "builder-01", "aaaa11112222", ApprovalStatus::Revoked),
            worker(2, "builder-01", "bbbb22223333", ApprovalStatus::Approved),
            worker(3, "builder-arm", "cccc33334444", ApprovalStatus::Approved),
        ];
        let shared = shared_names(&fleet);
        assert_eq!(shared, ["builder-01"].into_iter().collect());
        assert_eq!(
            worker_route(&fleet[1], shared.contains("builder-01")),
            Route::WorkerByFingerprint {
                fingerprint: "bbbb22223333".to_string(),
            }
        );
        // Everyone else still links by name.
        assert!(matches!(
            worker_route(&fleet[2], shared.contains("builder-arm")),
            Route::Worker { .. }
        ));
    }

    /// A name with a slash in it goes into the URL in one piece and comes back
    /// the same. Nothing stops `WORKER_NAME` containing one, and a name read
    /// back short is a page about the wrong machine.
    #[test]
    fn a_name_with_a_slash_survives_the_url() {
        let fleet = [worker(1, "ci/runner", "aaaa1111", ApprovalStatus::Approved)];
        let Route::Worker { name } = worker_route(&fleet[0], false) else {
            panic!("a unique name should link by name");
        };
        assert_eq!(name, vec!["ci".to_string(), "runner".to_string()]);
        assert_eq!(name.join("/"), "ci/runner");
        assert_eq!(name_segments("ci/runner"), name);
    }

    /// Short enough to read, and never longer than the fingerprint it came
    /// from -- the fixtures and tests use short ones.
    #[test]
    fn a_short_fingerprint_is_a_prefix() {
        assert_eq!(short_fingerprint("0123456789abcdef0123"), "0123456789ab");
        assert_eq!(short_fingerprint("abc"), "abc");
        assert_eq!(short_fingerprint(""), "");
    }

    fn decl(key: &str, category: &str) -> SettingDecl {
        SettingDecl {
            key: key.to_string(),
            kind: ValueKind::Text,
            description: String::new(),
            category: category.to_string(),
            default: None,
            env_var: None,
            applies: Applies::NextJob,
        }
    }

    /// A category is declared in two tables -- the protocol's and the
    /// executor's -- so grouping has to collect both under one heading rather
    /// than starting a second one when the category changes back.
    #[test]
    fn a_category_declared_twice_is_shown_once() {
        let declared = [
            decl("build_timeout", "Build limits"),
            decl("keyserver", "Signatures"),
            decl("build_memory_max", "Build limits"),
        ];
        let groups = group_by_category(&declared);
        let names: Vec<_> = groups.iter().map(|group| group.name.as_str()).collect();
        assert_eq!(names, ["Build limits", "Signatures"]);
        assert_eq!(groups[0].settings.len(), 2);
    }

    /// Categories appear in the order the worker first mentioned them, which is
    /// the order it grouped related settings in.
    use super::{Delivery, Drafts, SettingRow, applies_note, delivery, stage};
    use aurcache_client::WorkerConfigView;
    use aurcache_common::worker_config::{
        EffectiveConfig, EffectiveSetting, EffectiveSource, SettingStatus,
    };
    use dioxus::prelude::*;
    use std::collections::BTreeMap;

    /// Typing is staged as a change against what is stored here: the stored
    /// value again is no change, and emptying a stored value removes it.
    #[test]
    fn typing_stages_the_change_it_means() {
        assert_eq!(stage(None, "3"), Some(Some("3".to_string())));
        assert_eq!(stage(Some("3"), "3"), None);
        assert_eq!(stage(Some("3"), " 3 "), None, "whitespace is not a change");
        assert_eq!(stage(Some("3"), "4"), Some(Some("4".to_string())));
        assert_eq!(stage(Some("3"), ""), Some(None));
        assert_eq!(stage(None, "  "), None);
    }

    fn view(values: &[(&str, &str)], revision: &str, held: Option<&str>) -> WorkerConfigView {
        WorkerConfigView {
            worker_id: 1,
            settings: Vec::new(),
            effective: Some(EffectiveConfig {
                received_revision: held.map(str::to_string),
                settings: BTreeMap::new(),
            }),
            values: values
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            revision: revision.to_string(),
        }
    }

    #[test]
    fn delivery_compares_what_is_saved_with_what_the_worker_holds() {
        assert_eq!(
            delivery(&view(&[("a", "1")], "r2", Some("r2"))),
            Delivery::Current
        );
        assert_eq!(
            delivery(&view(&[("a", "1")], "r2", Some("r1"))),
            Delivery::Pending
        );
        // A worker that has taken nothing yet is behind only when there is
        // something to take.
        assert_eq!(
            delivery(&view(&[("a", "1")], "r1", None)),
            Delivery::Pending
        );
        assert_eq!(delivery(&view(&[], "r0", None)), Delivery::Current);
    }

    /// The warning that matters most: an immediate change reaches builds
    /// already running.
    #[test]
    fn an_immediate_setting_says_it_reaches_running_builds() {
        assert!(applies_note(Applies::Immediately).contains("already running"));
        assert!(applies_note(Applies::NextJob).contains("next build"));
    }

    /// A row is rendered through a host component: signals only exist inside
    /// a running runtime.
    #[component]
    fn RowHarness(
        effective: Option<EffectiveSetting>,
        stored: Option<String>,
        drafted: Option<Option<String>>,
    ) -> Element {
        let drafts = use_signal(|| {
            let mut drafts = Drafts::new();
            if let Some(change) = drafted.clone() {
                drafts.insert("concurrency".to_string(), change);
            }
            drafts
        });
        let mut decl = decl("concurrency", "Scheduling");
        decl.env_var = Some("WORKER_CONCURRENCY".to_string());
        decl.kind = ValueKind::Integer {
            min: Some(1),
            max: None,
        };
        decl.applies = Applies::Immediately;
        rsx! {
            table { tbody {
                SettingRow { decl, effective, stored, drafts }
            } }
        }
    }

    fn row(
        effective: EffectiveSetting,
        stored: Option<&str>,
        drafted: Option<Option<&str>>,
    ) -> String {
        let mut dom = VirtualDom::new_with_props(
            RowHarness,
            RowHarnessProps {
                effective: Some(effective),
                stored: stored.map(str::to_string),
                drafted: drafted.map(|d| d.map(str::to_string)),
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn running(value: &str, source: EffectiveSource, status: SettingStatus) -> EffectiveSetting {
        EffectiveSetting {
            value: Some(value.to_string()),
            source,
            status,
            reason: None,
        }
    }

    /// A pinned setting cannot be edited here -- a saved value would not take
    /// -- and the row names the variable doing the pinning.
    #[test]
    fn a_pinned_setting_is_not_editable_here() {
        let html = row(
            running("2", EffectiveSource::Env, SettingStatus::Applied),
            None,
            None,
        );
        assert!(html.contains("disabled"), "{html}");
        assert!(html.contains("pinned by WORKER_CONCURRENCY"), "{html}");
        assert!(!html.contains(">Reset<"), "{html}");
    }

    /// An unpinned setting with a value set here shows that value and offers
    /// to remove it; with none, the field is empty and says what an empty
    /// field means.
    #[test]
    fn a_value_set_here_can_be_edited_and_reset() {
        let html = row(
            running("3", EffectiveSource::Server, SettingStatus::Applied),
            Some("3"),
            None,
        );
        assert!(!html.contains("disabled"), "{html}");
        assert!(html.contains(">Reset<"), "{html}");
        assert!(html.contains("set here"), "{html}");

        let html = row(
            running("1", EffectiveSource::Default, SettingStatus::Applied),
            None,
            None,
        );
        assert!(html.contains("placeholder=\"1\""), "{html}");
        assert!(!html.contains(">Reset<"), "{html}");
    }

    /// A staged change says when it will take effect -- for this setting, at
    /// once, including for builds already running.
    #[test]
    fn a_staged_change_says_when_it_takes_effect() {
        let html = row(
            running("1", EffectiveSource::Default, SettingStatus::Applied),
            None,
            Some(Some("4")),
        );
        assert!(html.contains("already running"), "{html}");
        let untouched = row(
            running("1", EffectiveSource::Default, SettingStatus::Applied),
            None,
            None,
        );
        assert!(!untouched.contains("already running"), "{untouched}");
    }

    #[test]
    fn categories_keep_the_workers_order() {
        let declared = [
            decl("concurrency", "Scheduling"),
            decl("cache_ttl", "Caches"),
        ];
        let groups = group_by_category(&declared);
        assert_eq!(groups[0].name, "Scheduling");
        assert_eq!(groups[1].name, "Caches");
    }
}
