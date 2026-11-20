//! How a package is built: its platforms, makepkg flags and the settings it
//! may override for itself.

use crate::action::use_action;
use crate::api::client;
use crate::platforms::PlatformChecklist;
use crate::routes::Route;
use crate::widgets::Modal;
use aurcache_client::{ExtendedPackage, PackagePatch, Setting, SettingSource, SettingsEntry};
use aurcache_common::build_flags::OFFERED;
use dioxus::prelude::*;
use std::collections::BTreeSet;

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

/// The makepkg flags this package builds with, as read-only chips, and the
/// dialog that changes them.
///
/// The server keeps a package's flags in the order of [`OFFERED`], so the
/// chips come in that order as given.
#[component]
fn BuildFlagsField(pkgbase: String, flags: Vec<String>, on_changed: EventHandler<()>) -> Element {
    let mut editing = use_signal(|| false);
    // This component stays mounted when the route moves between packages, and
    // a dialog left open would then hold the last package's draft.
    use_effect(use_reactive(&pkgbase, move |_: String| {
        let mut editing = editing;
        editing.set(false);
    }));

    rsx! {
        div { class: "flex gap-2 py-1 text-sm",
            span { class: "opacity-60 w-24 shrink-0", "Flags" }
            div { class: "min-w-0 flex-1 flex flex-wrap items-center gap-1",
                if flags.is_empty() {
                    span { class: "opacity-60 text-xs", "makepkg defaults" }
                } else {
                    for flag in flags.iter() {
                        span { key: "{flag}", class: "badge badge-outline font-mono text-xs", "{flag}" }
                    }
                }
                button {
                    class: "btn btn-ghost btn-xs",
                    aria_label: "Edit build flags",
                    onclick: move |_| editing.set(true),
                    "Edit"
                }
            }
        }
        // Mounted only while open, so every opening starts from the saved
        // flags and a dismissed draft is gone with it.
        if editing() {
            BuildFlagsDialog {
                pkgbase,
                saved: flags,
                on_close: move |()| editing.set(false),
                on_saved: move |()| {
                    editing.set(false);
                    on_changed.call(());
                },
            }
        }
    }
}

/// One checkbox per offered flag, over a draft that only a Save sends.
///
/// The endpoint takes the whole list, so Save sends all of it at once.
#[component]
fn BuildFlagsDialog(
    pkgbase: String,
    /// The flags the package has now.
    saved: Vec<String>,
    /// Dismissed without saving.
    on_close: EventHandler<()>,
    /// The server took the new list.
    on_saved: EventHandler<()>,
) -> Element {
    let mut draft = use_signal(|| Draft::new(&saved));
    let action = use_action();
    let busy = action.busy();
    let changed = draft.read().differs(&saved);

    let save = {
        let pkgbase = pkgbase.clone();
        move |_| {
            let pkgbase = pkgbase.clone();
            async move {
                let patch = PackagePatch {
                    build_flags: Some(draft.read().flags()),
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
                    on_saved.call(());
                }
            }
        }
    };

    rsx! {
        Modal {
            open: true,
            label: "Build flags for {pkgbase}",
            on_close,
            locked: busy,
            h3 { class: "font-bold text-lg", "Build flags" }
            p { class: "text-sm opacity-70 pt-1 pb-3",
                "Passed to makepkg on every build of {pkgbase}. With none ticked, it builds as makepkg does by default."
            }
            {flag_list(&draft.read(), busy, EventHandler::new(move |tick: Tick| draft.with_mut(|d| d.set(tick.flag, tick.on))))}
            if let Some(message) = action.error() {
                div { class: "alert alert-error text-sm mt-3", role: "alert",
                    span { "{message}" }
                }
            }
            div { class: "modal-action",
                button {
                    class: "btn btn-sm",
                    disabled: busy,
                    onclick: move |_| on_close.call(()),
                    "Cancel"
                }
                button {
                    class: "btn btn-primary btn-sm",
                    disabled: busy || !changed,
                    onclick: save,
                    if busy {
                        span { class: "loading loading-spinner loading-xs" }
                    }
                    "Save"
                }
            }
        }
    }
}

/// A box ticked or unticked in [`flag_list`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tick {
    flag: &'static str,
    on: bool,
}

/// A row per offered flag, laid out as the add dialog's search results are:
/// the flag on one line, what it does under it.
///
/// Each row is the checkbox's label, so a click anywhere on it toggles the box.
fn flag_list(draft: &Draft, locked: bool, on_tick: EventHandler<Tick>) -> Element {
    rsx! {
        ul { class: "menu menu-sm p-0 max-h-[60vh] overflow-y-auto border border-base-300 rounded-box flex-nowrap",
            for offered in OFFERED {
                {
                    let id = checkbox_id(offered.flag);
                    rsx! {
                        li { key: "{offered.flag}",
                            label {
                                class: if locked { "flex items-start gap-3 cursor-not-allowed" } else { "flex items-start gap-3 cursor-pointer" },
                                input {
                                    r#type: "checkbox",
                                    class: "checkbox checkbox-sm mt-0.5",
                                    id: "{id}",
                                    aria_labelledby: "{id}-flag",
                                    aria_describedby: "{id}-summary",
                                    checked: draft.has(offered.flag),
                                    disabled: locked,
                                    onchange: move |e: FormEvent| {
                                        on_tick.call(Tick { flag: offered.flag, on: e.checked() });
                                    },
                                }
                                div { class: "flex flex-col gap-0.5 min-w-0",
                                    span { id: "{id}-flag", class: "font-mono", "{offered.flag}" }
                                    span { id: "{id}-summary", class: "text-xs opacity-60", "{offered.summary}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The element id of a flag's checkbox: `--nocheck` is `build-flag-nocheck`.
fn checkbox_id(flag: &str) -> String {
    format!("build-flag-{}", flag.trim_start_matches('-'))
}

/// The flags ticked in [`BuildFlagsDialog`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Draft {
    ticked: BTreeSet<String>,
}

impl Draft {
    /// A draft with the saved flags ticked.
    fn new(saved: &[String]) -> Self {
        Self {
            ticked: saved.iter().cloned().collect(),
        }
    }

    /// Tick `flag`, or untick it.
    fn set(&mut self, flag: &str, on: bool) {
        if on {
            self.ticked.insert(flag.to_string());
        } else {
            self.ticked.remove(flag);
        }
    }

    /// Whether `flag` is ticked.
    fn has(&self, flag: &str) -> bool {
        self.ticked.contains(flag)
    }

    /// The list to save: the ticked flags in the order of [`OFFERED`], the
    /// order the server keeps them in.
    fn flags(&self) -> Vec<String> {
        OFFERED
            .iter()
            .filter(|offered| self.has(offered.flag))
            .map(|offered| offered.flag.to_string())
            .collect()
    }

    /// Whether saving would store something other than `saved`.
    fn differs(&self, saved: &[String]) -> bool {
        self.flags() != saved
    }
}

#[cfg(test)]
mod tests {
    use super::{BuildFlagsDialog, BuildFlagsField, Draft, OFFERED};
    use dioxus::prelude::*;

    fn flags(list: &[&str]) -> Vec<String> {
        list.iter().map(|flag| (*flag).to_string()).collect()
    }

    #[test]
    fn the_list_to_save_follows_offered_order_not_ticking_order() {
        let mut draft = Draft::default();
        draft.set("--skippgpcheck", true);
        draft.set("--holdver", true);
        draft.set("--nocheck", true);
        assert_eq!(
            draft.flags(),
            flags(&["--nocheck", "--holdver", "--skippgpcheck"])
        );
    }

    #[test]
    fn unticking_takes_a_flag_off_the_list() {
        let mut draft = Draft::new(&flags(&["--nocheck", "--nodeps"]));
        draft.set("--nocheck", false);
        assert_eq!(draft.flags(), flags(&["--nodeps"]));
    }

    /// The server stores only offered flags, so neither does the list to save.
    #[test]
    fn the_list_to_save_holds_only_offered_flags() {
        let draft = Draft::new(&flags(&["--bogus", "--holdver"]));
        assert_eq!(draft.flags(), flags(&["--holdver"]));
    }

    #[test]
    fn a_draft_differs_only_once_it_would_save_something_else() {
        let saved = flags(&["--nocheck"]);
        let mut draft = Draft::new(&saved);
        assert!(!draft.differs(&saved));

        draft.set("--holdver", true);
        assert!(draft.differs(&saved));

        draft.set("--holdver", false);
        assert!(!draft.differs(&saved), "ticked and unticked again");
    }

    /// An empty list is how a package goes back to makepkg's defaults, so
    /// unticking the last flag is a change to send.
    #[test]
    fn unticking_every_flag_is_a_change() {
        let saved = flags(&["--nocheck"]);
        let mut draft = Draft::new(&saved);
        draft.set("--nocheck", false);
        assert!(draft.differs(&saved));
        assert!(draft.flags().is_empty());
    }

    #[component]
    fn Dialog(saved: Vec<String>) -> Element {
        rsx! {
            BuildFlagsDialog {
                pkgbase: "hello",
                saved,
                on_close: move |()| {},
                on_saved: move |()| {},
            }
        }
    }

    #[component]
    fn Field(flags: Vec<String>) -> Element {
        rsx! {
            BuildFlagsField { pkgbase: "hello", flags, on_changed: move |()| {} }
        }
    }

    fn render_dialog(saved: &[&str]) -> String {
        let mut dom = VirtualDom::new_with_props(
            Dialog,
            DialogProps {
                saved: flags(saved),
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn render_field(list: &[&str]) -> String {
        let mut dom = VirtualDom::new_with_props(Field, FieldProps { flags: flags(list) });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The text as it appears in markup, where an apostrophe is escaped.
    fn escaped(text: &str) -> String {
        text.replace('\'', "&#39;")
    }

    #[test]
    fn the_dialog_offers_every_flag_with_what_it_does() {
        let html = render_dialog(&[]);
        for offered in OFFERED {
            assert!(
                html.contains(offered.flag),
                "{} missing:\n{html}",
                offered.flag
            );
            assert!(
                html.contains(&escaped(offered.summary)),
                "the summary of {} missing:\n{html}",
                offered.flag
            );
        }
        assert!(
            html.contains("aria-label=\"Build flags for hello\""),
            "{html}"
        );
    }

    #[test]
    fn the_dialog_starts_with_the_saved_flags_ticked() {
        let html = render_dialog(&["--holdver"]);
        let ticked: Vec<&str> = html
            .split("<input")
            .skip(1)
            .filter(|input| input.split('>').next().unwrap_or("").contains("checked"))
            .collect();
        assert_eq!(ticked.len(), 1, "{html}");
        assert!(ticked[0].contains("id=\"build-flag-holdver\""), "{html}");
    }

    /// Nothing has changed yet, so there is nothing to save.
    #[test]
    fn save_waits_for_a_change() {
        let html = render_dialog(&["--holdver"]);
        let save = html
            .split("<button")
            .find(|button| button.contains("Save"))
            .expect("a Save button");
        assert!(
            save.split('>').next().unwrap_or("").contains("disabled"),
            "{html}"
        );
    }

    #[test]
    fn the_summary_shows_the_set_flags_as_chips() {
        let html = render_field(&["--nocheck", "--holdver"]);
        assert!(html.contains(">--nocheck</span>"), "{html}");
        assert!(html.contains(">--holdver</span>"), "{html}");
        assert!(!html.contains("makepkg defaults"), "{html}");
        // The dialog is not mounted until Edit is clicked.
        assert!(!html.contains("--nodeps"), "{html}");
    }

    #[test]
    fn the_summary_names_the_defaults_when_no_flag_is_set() {
        let html = render_field(&[]);
        assert!(html.contains("makepkg defaults"), "{html}");
        assert!(html.contains(">Edit</button>"), "{html}");
    }
}
