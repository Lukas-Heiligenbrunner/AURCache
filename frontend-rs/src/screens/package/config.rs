//! How a package is built: its platforms, makepkg flags and the settings it
//! may override for itself.

use crate::action::use_action;
use crate::api::client;
use crate::platforms::PlatformChecklist;
use crate::routes::Route;
use aurcache_client::{ExtendedPackage, PackagePatch, Setting, SettingSource, SettingsEntry};
use dioxus::prelude::*;

#[component]
pub(super) fn BuildConfigCard(pkg: ExtendedPackage, on_changed: EventHandler<()>) -> Element {
    let entry = use_resource(use_reactive(&pkg.name, move |pkgbase| async move {
        client()?
            .settings(Some(&pkgbase))
            .await
            .map_err(|error| error.to_string())
    }));
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base", "Build config" }
                PlatformField {
                    pkgbase: pkg.name.clone(),
                    selected: pkg.selected_platforms.clone(),
                    on_changed,
                }
                BuildFlagsField {
                    pkgbase: pkg.name.clone(),
                    // A package with no flags stores an empty string, which the
                    // API splits on `;` into one empty flag rather than none.
                    // Left in, it renders as a chip with no label.
                    flags: pkg
                        .selected_build_flags
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|flag| !flag.trim().is_empty())
                        .collect(),
                    on_changed,
                }
                PersistBuildDirField { key: "persist-{pkg.name}", pkgbase: pkg.name.clone(), entry }
                PackageLimitField {
                    key: "artifact-{pkg.name}",
                    pkgbase: pkg.name.clone(),
                    entry,
                    limit: PackageLimit::ArtifactSize,
                }
                PackageLimitField {
                    key: "timeout-{pkg.name}",
                    pkgbase: pkg.name.clone(),
                    entry,
                    limit: PackageLimit::JobTimeout,
                }
                // Editing the sources is what creates or clears the package's
                // patch, so the button says whether there is one rather than a
                // row of its own saying so beside it.
                Link {
                    class: "btn btn-sm btn-block mt-2",
                    to: Route::PackageSource { pkgbase: pkg.name.clone(), path: vec![] },
                    if pkg.has_patch { "Edit sources (patch exists)" } else { "Edit sources" }
                }
                // The per-package makepkg.conf/pacman.conf overrides. They
                // need a page of their own — two full-height editors — and
                // without a way through it is reachable only by typing its URL.
                Link {
                    class: "btn btn-sm btn-block mt-2",
                    to: Route::PackageConfigFiles { pkgbase: pkg.name },
                    "Config files"
                }
            }
        }
    }
}

/// Whether this package keeps its build tree between builds.
///
/// A boolean with no Save step: toggling applies immediately, which is all the
/// setting warrants. The checkbox flips on click and a re-read after the write
/// keeps the row showing what the server actually holds, and where it now comes
/// from — a package override, or a value inherited from elsewhere.
///
/// Read through the settings sub-resource rather than the package object:
/// settings are fetched separately, and folding one of them into the package
/// payload would make every package request carry it.
#[component]
fn PersistBuildDirField(
    pkgbase: String,
    mut entry: Resource<Result<aurcache_client::ApplicationSettings, String>>,
) -> Element {
    use crate::setting_editor::{
        MutationOutcome, SettingEdit, SettingScope, use_editor_draft, use_setting_mutations,
    };
    let controller = use_setting_mutations();
    let busy = controller.busy;
    let error = move || (controller.status)().and_then(Result::err);
    let loaded = entry
        .read()
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .map(|settings| settings.persistent_builddir.value.to_string());
    let mut draft = use_editor_draft(loaded);
    let scope = SettingScope::Package(pkgbase);
    let save_scope = scope.clone();
    let set = move |enabled: bool| {
        let scope = save_scope.clone();
        async move {
            let value = enabled.to_string();
            draft.with_mut(|draft| draft.edit(value.clone()));
            if controller
                .apply(
                    &scope,
                    SettingEdit::set(Setting::PersistentBuilddir, value).editing(draft),
                )
                .await
                == MutationOutcome::Applied
            {
                entry.restart();
            }
        }
    };
    let reset = move |_| {
        let scope = scope.clone();
        async move {
            if controller
                .apply(
                    &scope,
                    SettingEdit::reset(Setting::PersistentBuilddir).editing(draft),
                )
                .await
                == MutationOutcome::Applied
            {
                entry.restart();
            }
        }
    };

    rsx! {
        div { class: "flex gap-2 py-1 text-sm items-center",
            span { class: "opacity-60 w-32 shrink-0", "Persist build dir" }
            div { class: "min-w-0 flex-1",
                match &*entry.read_unchecked() {
                    None => rsx! {
                        span { class: "loading loading-spinner loading-xs" }
                    },
                    Some(Err(e)) => rsx! {
                        span { class: "text-xs text-error", "Could not load the setting: {e}" }
                    },
                    Some(Ok(settings)) => {
                        let source = settings.persistent_builddir.source;
                        let locked = source == SettingSource::Env;
                        rsx! {
                            div { class: "flex items-center gap-2 flex-wrap",
                                input {
                                    r#type: "checkbox",
                                    class: "toggle toggle-primary",
                                    disabled: locked || busy(),
                                    checked: draft().text() == "true",
                                    onchange: move |e: FormEvent| {
                                        set(e.checked())
                                    },
                                }
                                if source == SettingSource::Package && !locked && !busy() {
                                    button {
                                        class: "btn btn-ghost btn-xs",
                                        title: "Discard this package's override and inherit the default again",
                                        onclick: reset,
                                        "Reset"
                                    }
                                }
                                if let Some(message) = error() {
                                    span { class: "text-xs text-error", "{message}" }
                                }
                            }
                            if locked
                                && let Some(name) = Setting::PersistentBuilddir.meta().env_name
                            {
                                p { class: "text-xs text-warning", "unset ${name} to allow control here" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A server setting this package may set for itself: the largest file its
/// builds may upload, or how long one may run.
///
/// Both resolve per package (see `Setting::MaxArtifactSize`), so one outsized
/// or slow package can be allowed more without raising the limit for all.
/// Shown as it is written (`20G`, `3h`), with where the value comes from;
/// typing a new one offers Save, and a package override offers Reset.
#[component]
fn PackageLimitField(
    pkgbase: String,
    mut entry: Resource<Result<aurcache_client::ApplicationSettings, String>>,
    limit: PackageLimit,
) -> Element {
    let setting = limit.setting();
    use crate::setting_editor::{
        MutationOutcome, SettingEdit, SettingScope, use_editor_draft, use_setting_mutations,
    };
    let controller = use_setting_mutations();
    let busy = controller.busy;
    let error = move || (controller.status)().and_then(Result::err);
    let loaded = entry
        .read()
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .map(|settings| limit.shown(settings).value);
    let mut draft = use_editor_draft(loaded);
    let scope = SettingScope::Package(pkgbase);
    let save_scope = scope.clone();
    let save = move |_| {
        let scope = save_scope.clone();
        async move {
            let value = draft().text().trim().to_owned();
            if controller
                .apply(&scope, SettingEdit::set(setting, value).editing(draft))
                .await
                == MutationOutcome::Applied
            {
                entry.restart();
            }
        }
    };
    let reset = move |_| {
        let scope = scope.clone();
        async move {
            if controller
                .apply(&scope, SettingEdit::reset(setting).editing(draft))
                .await
                == MutationOutcome::Applied
            {
                entry.restart();
            }
        }
    };

    rsx! {
        div { class: "flex gap-2 py-1 text-sm items-center",
            span { class: "opacity-60 w-32 shrink-0", "{limit.label()}" }
            div { class: "min-w-0 flex-1",
                match &*entry.read_unchecked() {
                    None => rsx! { span { class: "loading loading-spinner loading-xs" } },
                    Some(Err(e)) => rsx! {
                        span { class: "text-xs text-error", "Could not load the setting: {e}" }
                    },
                    Some(Ok(settings)) => {
                        let SettingsEntry { value: current, source } = limit.shown(settings);
                        // No lock for an environment variable: a package's own
                        // value outranks it (Package -> Env -> Global -> Default),
                        // so an override here still applies.
                        let changed = draft().text().trim() != current;
                        rsx! {
                            div { class: "flex items-center gap-2 flex-wrap",
                                input {
                                    r#type: "text",
                                    class: "input input-bordered input-xs w-24 font-mono",
                                    placeholder: limit.placeholder(),
                                    disabled: busy(),
                                    value: draft().text(),
                                    oninput: move |e| draft.with_mut(|draft| draft.edit(e.value())),
                                }
                                span {
                                    class: "badge badge-ghost badge-sm",
                                    title: "Where this value comes from",
                                    match source {
                                        SettingSource::Package => "this package",
                                        SettingSource::Global => "global",
                                        SettingSource::Env => "environment",
                                        SettingSource::Default => "default",
                                    }
                                }
                                if changed {
                                    button {
                                        class: "btn btn-primary btn-xs",
                                        disabled: busy(),
                                        onclick: save,
                                        "Save"
                                    }
                                }
                                if source == SettingSource::Package && !busy() {
                                    button {
                                        class: "btn btn-ghost btn-xs",
                                        title: "Discard this package's limit and use the global one again",
                                        onclick: reset,
                                        "Reset"
                                    }
                                }
                            }
                            if let Some(message) = error() {
                                p { class: "text-xs text-error", "{message}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The limits a package may set for itself, each shown by [`PackageLimitField`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PackageLimit {
    ArtifactSize,
    JobTimeout,
}

impl PackageLimit {
    const fn setting(self) -> Setting {
        match self {
            Self::ArtifactSize => Setting::MaxArtifactSize,
            Self::JobTimeout => Setting::JobTimeout,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::ArtifactSize => "Max artifact size",
            Self::JobTimeout => "Job timeout",
        }
    }

    const fn placeholder(self) -> &'static str {
        match self {
            Self::ArtifactSize => "20G",
            Self::JobTimeout => "1h",
        }
    }

    /// The value as it is written, and where it comes from.
    fn shown(self, settings: &aurcache_client::ApplicationSettings) -> SettingsEntry<String> {
        match self {
            Self::ArtifactSize => SettingsEntry {
                value: aurcache_common::units::format_size(settings.max_artifact_size.value),
                source: settings.max_artifact_size.source,
            },
            Self::JobTimeout => SettingsEntry {
                value: aurcache_common::units::format_duration(u64::from(
                    settings.job_timeout.value,
                )),
                source: settings.job_timeout.source,
            },
        }
    }
}

/// The platforms a package is built for, with an inline editor.
///
/// Changing this changes which dependencies are required — a PKGBUILD can
/// declare `depends_aarch64` separately — so the server re-resolves the
/// dependency graph on save. That is why the page reloads afterwards rather
/// than patching the field in place.
#[component]
pub fn PlatformField(
    pkgbase: String,
    selected: Vec<String>,
    on_changed: EventHandler<()>,
) -> Element {
    let mut editing = use_signal(|| false);
    let mut draft = use_signal(|| selected.clone());
    let action = use_action();

    // Reset the draft each time the editor opens, so a cancelled edit does not
    // linger into the next one.
    let start = {
        let selected = selected.clone();
        move |_| {
            draft.set(selected.clone());
            action.clear();
            editing.set(true);
        }
    };

    rsx! {
        div { class: "flex gap-2 py-1 text-sm",
            span { class: "opacity-60 w-24 shrink-0", "Platforms" }
            div { class: "min-w-0 flex-1",
                if editing() {
                    div { class: "flex flex-col gap-1",
                        PlatformChecklist {
                            selected: draft(),
                            onchange: move |next| draft.set(next),
                        }
                        if let Some(message) = action.error() {
                            span { class: "text-xs text-error", "{message}" }
                        }
                        div { class: "flex gap-2 pt-1",
                            button {
                                class: "btn btn-primary btn-xs",
                                // A package built for nothing would never build
                                // again, so saving an empty set is refused
                                // rather than accepted and puzzled over later.
                                disabled: action.busy() || draft().is_empty(),
                                onclick: move |_| {
                                    let pkgbase = pkgbase.clone();
                                    async move {
                                        let patch = PackagePatch {
                                            platforms: Some(draft()),
                                            ..Default::default()
                                        };
                                        let saved = action
                                            .run(async {
                                                client()?
                                                    .patch_package(&pkgbase, &patch)
                                                    .await
                                                    .map_err(|e| e.to_string())
                                            })
                                            .await;
                                        if saved.is_some() {
                                            editing.set(false);
                                            on_changed.call(());
                                        }
                                    }
                                },
                                "Save"
                            }
                            button {
                                class: "btn btn-ghost btn-xs",
                                disabled: action.busy(),
                                onclick: move |_| editing.set(false),
                                "Cancel"
                            }
                        }
                    }
                } else {
                    div { class: "flex items-center gap-2 flex-wrap",
                        span { class: "font-mono text-xs", {selected.join(", ")} }
                        button {
                            class: "btn btn-ghost btn-xs",
                            onclick: start,
                            "Change"
                        }
                    }
                }
            }
        }
    }
}

/// The makepkg flags this package builds with, as chips.
///
/// Free-form rather than a fixed set: they are passed to makepkg, which has far
/// more of them than a checklist would be honest about. Every edit saves the
/// whole list, because that is what the endpoint takes — there is no
/// add-one/remove-one operation to mirror.
#[component]
fn BuildFlagsField(pkgbase: String, flags: Vec<String>, on_changed: EventHandler<()>) -> Element {
    let mut draft = use_signal(String::new);
    let action = use_action();

    let current = use_signal(|| flags.clone());
    let current_pkgbase = use_signal(|| pkgbase.clone());
    // Follow the props: this component stays mounted when the route moves
    // between packages, so the signals need to track the current package.
    use_effect(use_reactive(&flags, move |flags| {
        let mut current = current;
        current.set(flags);
    }));
    use_effect(use_reactive(&pkgbase, move |pkgbase: String| {
        let mut current_pkgbase = current_pkgbase;
        current_pkgbase.set(pkgbase);
    }));

    let save = move |next: Vec<String>| async move {
        let patch = PackagePatch {
            build_flags: Some(next),
            ..Default::default()
        };
        let saved = action
            .run(async {
                client()?
                    .patch_package(&current_pkgbase(), &patch)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await;
        if saved.is_some() {
            draft.set(String::new());
            on_changed.call(());
        }
    };

    // Adding a flag already present would save a list with a duplicate in it,
    // which makepkg would then see twice.
    let entered = draft().trim().to_string();
    let can_add = !entered.is_empty() && !current().contains(&entered) && !action.busy();

    let add = move |()| async move {
        let entered = draft().trim().to_string();
        if entered.is_empty() || current().contains(&entered) {
            return;
        }
        let mut next = current();
        next.push(entered);
        save(next).await;
    };

    rsx! {
        div { class: "flex gap-2 py-1 text-sm",
            span { class: "opacity-60 w-24 shrink-0", "Flags" }
            div { class: "min-w-0 flex-1 flex flex-col gap-2",
                if current().is_empty() {
                    span { class: "opacity-60 text-xs italic",
                        "No build flags. makepkg runs with its own defaults."
                    }
                } else {
                    div { class: "flex flex-wrap gap-1",
                        for flag in current() {
                            span {
                                key: "{flag}",
                                class: "badge badge-outline gap-1 font-mono text-xs",
                                "{flag}"
                                button {
                                    class: "opacity-60 hover:opacity-100",
                                    disabled: action.busy(),
                                    aria_label: "Remove {flag}",
                                    onclick: {
                                        move |_| {
                                            let flag = flag.clone();
                                            async move {
                                                let next = current()
                                                    .into_iter()
                                                    .filter(|f| *f != flag)
                                                    .collect();
                                                save(next).await;
                                            }
                                        }
                                    },
                                    "✕"
                                }
                            }
                        }
                    }
                }

                div { class: "flex gap-2",
                    input {
                        r#type: "text",
                        class: "input input-bordered input-xs font-mono w-48",
                        placeholder: "--nocheck",
                        value: "{draft}",
                        disabled: action.busy(),
                        oninput: move |e| draft.set(e.value()),
                        onkeydown: move |e: KeyboardEvent| async move {
                            if e.key() == Key::Enter {
                                add(()).await;
                            }
                        },
                    }
                    button {
                        class: "btn btn-xs",
                        disabled: !can_add,
                        onclick: move |_| add(()),
                        "Add"
                    }
                }

                if let Some(message) = action.error() {
                    span { class: "text-xs text-error", "{message}" }
                }
            }
        }
    }
}
