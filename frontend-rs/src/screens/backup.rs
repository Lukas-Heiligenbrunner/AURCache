//! Exporting and restoring an instance's configuration, from the settings page.
//!
//! Both directions are one card with two dialogs, because they are one job seen
//! from two ends: this is where you go when you are moving a server, taking a
//! backup, or putting one back.

use crate::screens::settings::Section;
use aurcache_client::{ExistingPackagePolicy, RestoreEntry, RestoreOutcome, SecretsPolicy};
use dioxus::html::{FileData, HasFileData};
use dioxus::prelude::*;

/// The most a dump is allowed to be before the browser refuses to read it.
///
/// Matches the server's own limit. A lite dump of a very large instance is a
/// few hundred kilobytes of compressed text, so this only stops someone
/// dropping an unrelated file and waiting for nothing.
const MAX_DUMP_BYTES: usize = 64 * 1024 * 1024;

#[component]
pub fn BackupSection() -> Element {
    let mut dumping = use_signal(|| false);
    let mut restoring = use_signal(|| false);

    rsx! {
        Section { title: "Backup",
            div { class: "py-2 space-y-3",
                p { class: "text-sm opacity-70",
                    "A dump carries what you configured -- packages, their sources and \
                     patches, settings, and the workers you trust. Build history and the \
                     packages themselves are rebuilt, not carried."
                }
                div { class: "flex gap-2",
                    button {
                        class: "btn btn-sm",
                        onclick: move |_| dumping.set(true),
                        "Export…"
                    }
                    button {
                        class: "btn btn-sm",
                        onclick: move |_| restoring.set(true),
                        "Restore…"
                    }
                }
            }
        }
        DumpDialog { open: dumping }
        RestoreDialog { open: restoring }
    }
}

/// Choosing what an export carries.
///
/// A plain link rather than a fetch: the browser already holds the session, and
/// letting it do the download means no copy of the archive passes through this
/// page's memory -- which for a dump carrying a CA private key is worth having.
#[component]
fn DumpDialog(open: Signal<bool>) -> Element {
    let mut with_secrets = use_signal(|| false);

    let close = move |_| {
        open.set(false);
        with_secrets.set(false);
    };

    rsx! {
        div { class: if open() { "modal modal-open" } else { "modal" },
            div { class: "modal-box",
                h3 { class: "font-bold text-lg", "Export configuration" }
                div { class: "py-4 space-y-3",
                    label { class: "flex items-start gap-3 cursor-pointer",
                        input {
                            r#type: "checkbox",
                            class: "checkbox checkbox-sm mt-0.5",
                            aria_label: "Include secrets",
                            checked: with_secrets(),
                            onchange: move |e| with_secrets.set(e.checked()),
                        }
                        div {
                            div { class: "text-sm", "Include secrets" }
                            div { class: "text-xs opacity-60",
                                "The CA private key, the certificates your workers hold, and \
                                 API token hashes. Restoring these makes a move invisible to \
                                 your workers: they reconnect instead of re-enrolling."
                            }
                        }
                    }
                    // Shown only once the box is ticked. A warning that is
                    // always on screen is one nobody reads by the time it
                    // matters.
                    if with_secrets() {
                        div { class: "alert alert-warning text-sm",
                            div {
                                div { class: "font-semibold", "This file is a credential." }
                                div { class: "text-xs",
                                    "Anyone holding it can act as a build worker for this \
                                     server, for as long as this CA lives. Keep it as you \
                                     would keep a private key."
                                }
                            }
                        }
                    }
                }
                div { class: "modal-action",
                    button { class: "btn btn-ghost", onclick: close, "Cancel" }
                    a {
                        class: if with_secrets() { "btn btn-warning" } else { "btn btn-primary" },
                        href: if with_secrets() {
                            "/api/dump?include_secrets=true"
                        } else {
                            "/api/dump"
                        },
                        // The server names the file; this only asks the browser
                        // to save rather than navigate.
                        download: "",
                        onclick: close,
                        if with_secrets() { "Download with secrets" } else { "Download" }
                    }
                }
            }
            div { class: "modal-backdrop", onclick: close }
        }
    }
}

/// Choosing a file and what to do with it.
#[component]
fn RestoreDialog(open: Signal<bool>) -> Element {
    let mut file_name = use_signal(String::new);
    // `Bytes`, not `Vec<u8>`: the file reader hands back `Bytes`, signal
    // reads then cost a refcount bump rather than a copy, and the client
    // streams them into the request body without copying either.
    let mut bytes = use_signal(bytes::Bytes::new);
    let on_existing = use_signal(ExistingPackagePolicy::default);
    let clear = use_signal(|| false);
    let secrets = use_signal(|| false);
    let mut hovering = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);
    let mut preview = use_signal(Vec::<RestoreEntry>::new);
    // Where a started restore goes once the dialog closes.
    let jobs = crate::progress::use_jobs();

    let mut reset = move || {
        file_name.set(String::new());
        bytes.set(bytes::Bytes::new());
        preview.set(Vec::new());
        error.set(None);
        hovering.set(false);
        busy.set(false);
    };

    let close = move |_| {
        open.set(false);
        reset();
    };

    // Reading the file is the same whether it was picked or dropped, so both
    // events land here. Only the first: a restore takes one dump, and quietly
    // using the first of several dropped files is better than failing on a
    // stray second one.
    let take_file = move |files: Vec<FileData>| async move {
        hovering.set(false);
        let Some(file) = files.into_iter().next() else {
            return;
        };
        let name = file.name();
        // Checked before reading rather than after: there is no reason to pull
        // a gigabyte into the page to discover it is too big.
        if file.size() as usize > MAX_DUMP_BYTES {
            error.set(Some(format!(
                "{name} is larger than this server will accept."
            )));
            return;
        }
        match file.read_bytes().await {
            Ok(content) => {
                // A newly chosen file invalidates whatever the last one
                // previewed; showing the old result against the new name would
                // be worse than showing nothing.
                preview.set(Vec::new());
                error.set(None);
                file_name.set(name);
                bytes.set(content);
            }
            Err(e) => error.set(Some(format!("{name} could not be read: {e}"))),
        }
    };

    let run = move |dry_run: bool| async move {
        if bytes().is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        let client = match crate::api::client() {
            Ok(client) => client,
            Err(e) => {
                error.set(Some(e));
                busy.set(false);
                return;
            }
        };
        let secrets_policy = if secrets() {
            SecretsPolicy::Copy
        } else {
            SecretsPolicy::Ignore
        };
        match client
            .restore(bytes(), dry_run, on_existing(), clear(), secrets_policy)
            .await
        {
            Ok(accepted) => {
                if dry_run {
                    preview.set(accepted.preview);
                } else if let Some(job_id) = accepted.job_id {
                    // Handed to the progress card and the dialog closes, rather
                    // than holding the page until every package is imported. A
                    // restore of a few hundred packages runs for minutes, and
                    // there is no reason the rest of the site -- adding a
                    // package included -- should be unreachable meanwhile.
                    //
                    // The same handover an add does, for the same reason and
                    // through the same machinery: the job is already detached
                    // server-side and its progress is a log read by offset, so
                    // nothing is lost by not watching from here.
                    crate::progress::watch_restore(
                        jobs,
                        job_id,
                        accepted.total,
                        format!("Restoring {}", file_name()),
                    );
                    open.set(false);
                    reset();
                }
            }
            Err(e) => error.set(Some(e.to_string())),
        }
        busy.set(false);
    };

    let blocked = move || {
        preview()
            .iter()
            .any(|e| matches!(e.outcome, RestoreOutcome::Failed { .. }))
    };

    rsx! {
        div { class: if open() { "modal modal-open" } else { "modal" },
            div { class: "modal-box max-w-2xl",
                h3 { class: "font-bold text-lg", "Restore from a dump" }
                div { class: "py-4 space-y-4",
                    // The drop target is also the picker: one region that
                    // accepts a file however you happen to give it one.
                    label {
                        class: if hovering() {
                            "flex flex-col items-center justify-center gap-1 border-2 border-dashed border-primary bg-primary/10 rounded-box py-6 cursor-pointer"
                        } else {
                            "flex flex-col items-center justify-center gap-1 border-2 border-dashed border-base-300 rounded-box py-6 cursor-pointer hover:border-base-content/40"
                        },
                        ondragover: move |e| {
                            // Without this the browser navigates to the file
                            // instead of offering it to the page.
                            e.prevent_default();
                            hovering.set(true);
                        },
                        ondragleave: move |_| hovering.set(false),
                        ondrop: move |e| async move {
                            // Without this the browser navigates to the file
                            // instead of offering it to the page.
                            e.prevent_default();
                            take_file(e.files()).await;
                        },
                        input {
                            r#type: "file",
                            class: "hidden",
                            accept: ".gz,.tar.gz,application/gzip",
                            onchange: move |e| async move {
                                take_file(e.files()).await;
                            },
                        }
                        if file_name().is_empty() {
                            span { class: "text-sm", "Drop a dump here, or click to choose one" }
                            span { class: "text-xs opacity-60", "aurcache-dump-….tar.gz" }
                        } else {
                            span { class: "font-mono text-sm break-all", "{file_name()}" }
                            span { class: "text-xs opacity-60", "Click to choose a different file" }
                        }
                    }

                    RestoreOptionsForm { on_existing, clear, secrets }

                    if let Some(message) = error() {
                        div { class: "alert alert-error text-sm", span { "{message}" } }
                    }
                    if !preview().is_empty() {
                        RestoreReport { entries: preview(), blocked: blocked() }
                    }
                }
                div { class: "modal-action",
                    button { class: "btn btn-ghost", disabled: busy(), onclick: close, "Close" }
                    button {
                        class: "btn",
                        disabled: busy() || bytes().is_empty(),
                        onclick: move |_| run(true),
                        "Preview"
                    }
                    button {
                        class: if clear() { "btn btn-warning" } else { "btn btn-primary" },
                        disabled: busy() || bytes().is_empty() || blocked(),
                        onclick: move |_| run(false),
                        if busy() { "Working…" } else if clear() { "Replace everything" } else { "Restore" }
                    }
                }
            }
            div { class: "modal-backdrop", onclick: close }
        }
    }
}

#[component]
fn RestoreOptionsForm(
    on_existing: Signal<ExistingPackagePolicy>,
    clear: Signal<bool>,
    secrets: Signal<bool>,
) -> Element {
    rsx! {
        div { class: "space-y-3",
            label { class: "form-control",
                span { class: "label-text text-sm", "Packages that are already here" }
                select {
                    class: "select select-bordered select-sm",
                    disabled: clear(),
                    value: on_existing().as_str(),
                    onchange: move |e| on_existing.set(e.value().parse().unwrap_or_default()),
                    option { value: ExistingPackagePolicy::Skip.as_str(), "Leave them alone" }
                    option { value: ExistingPackagePolicy::Overwrite.as_str(), "Replace with the dump's" }
                    option {
                        value: ExistingPackagePolicy::MergePatches.as_str(),
                        "Leave them, but take patches they lack"
                    }
                }
                // With nothing left to collide with, the choice above is not a
                // choice; saying so beats leaving a control that does nothing.
                if clear() {
                    span { class: "text-xs opacity-60",
                        "Nothing will already be here: everything is removed first."
                    }
                }
            }

            label { class: "flex items-start gap-3 cursor-pointer",
                input {
                    r#type: "checkbox",
                    class: "checkbox checkbox-sm mt-0.5",
                    aria_label: "Replace everything",
                    checked: clear(),
                    onchange: move |e| clear.set(e.checked()),
                }
                div {
                    div { class: "text-sm", "Replace everything" }
                    div { class: "text-xs opacity-60",
                        "Remove every package, setting and worker here first, so this \
                         instance ends up as the dump describes and nothing else."
                    }
                }
            }

            label { class: "flex items-start gap-3 cursor-pointer",
                input {
                    r#type: "checkbox",
                    class: "checkbox checkbox-sm mt-0.5",
                    aria_label: "Take the dump's secrets",
                    checked: secrets(),
                    onchange: move |e| secrets.set(e.checked()),
                }
                div {
                    div { class: "text-sm", "Take the dump's secrets" }
                    div { class: "text-xs opacity-60",
                        "Replaces this server's CA with the dump's. Every certificate your \
                         current workers hold was signed by the old one and stops working; \
                         they re-enrol. Ignored if the dump carries no secrets."
                    }
                }
            }
        }
    }
}

/// What an import did, or would do.
#[component]
fn RestoreReport(entries: Vec<RestoreEntry>, blocked: bool) -> Element {
    rsx! {
        div {
            if blocked {
                div { class: "alert alert-warning text-sm mb-2",
                    span { "This dump cannot be applied as configured. See below." }
                }
            }
            ul { class: "menu menu-sm p-0 max-h-64 overflow-y-auto border border-base-300 rounded-box flex-nowrap",
                for (index, entry) in entries.into_iter().enumerate() {
                    // Indexed as well as named: nothing stops two entries
                    // sharing a package and an outcome, and duplicate keys
                    // make the framework reuse the wrong row.
                    li { key: "{index}-{entry.pkgbase}-{entry.outcome.label()}",
                        div { class: "flex flex-col items-start gap-0.5",
                            div { class: "flex items-baseline gap-2",
                                span { class: "badge badge-xs {outcome_class(&entry.outcome)}",
                                    {entry.outcome.label()}
                                }
                                span { class: "font-mono text-sm break-all", "{entry.pkgbase}" }
                            }
                            if let RestoreOutcome::Failed { error } = &entry.outcome {
                                span { class: "text-xs opacity-70 text-left", "{error}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn outcome_class(outcome: &RestoreOutcome) -> &'static str {
    match outcome {
        RestoreOutcome::Imported | RestoreOutcome::PatchAdopted => "badge-success",
        RestoreOutcome::Overwritten => "badge-warning",
        RestoreOutcome::Skipped => "badge-ghost",
        RestoreOutcome::Failed { .. } => "badge-error",
    }
}
