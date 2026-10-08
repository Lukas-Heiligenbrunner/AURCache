//! The two config files every build runs against.
//!
//! `makepkg.conf` and `pacman.conf` are settings like any other — they resolve
//! through the same hierarchy, and are read and written through the same
//! endpoints — but they are files rather than values, so they get a page with
//! room to edit them instead of a row on the settings list.
//!
//! Neither has an environment variable behind it, so unlike everything on the
//! settings page these can only be stored or unset. "Unset" means the builder
//! image's own copy, which is why a reset is worth offering: it is the only way
//! back to whatever the image ships.

use crate::listing::ListHeader;
use aurcache_client::SettingSource;
use dioxus::prelude::*;

/// The files, in the order they are shown, as `(setting key, file name)`.
///
/// The key is the server's; the name is what the builder calls the file. They
/// differ, and showing the key would be showing an implementation detail.
const FILES: [(&str, &str); 2] = [
    ("makepkg_conf", "makepkg.conf"),
    ("pacman_conf", "pacman.conf"),
];

#[component]
pub fn ConfigFiles() -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Config files" }
                p { class: "text-xs opacity-60 max-w-prose -mt-1",
                    "Used by every build. Left unset, each falls back to the copy the builder image ships."
                }
                ConfigFileTabs { pkgbase: None }
            }
        }
    }
}

/// The two files, as tabs, for one scope.
///
/// Shared with the per-package settings page rather than reimplemented there:
/// both scopes are the same two settings behind the same endpoints, differing
/// only in which row is written. `pkgbase` of `None` is the server-wide file.
#[component]
pub fn ConfigFileTabs(pkgbase: Option<String>) -> Element {
    let mut showing = use_signal(|| FILES[0].0.to_string());

    rsx! {
        div { role: "tablist", class: "tabs tabs-bordered mt-2",
            for (key, name) in FILES {
                button {
                    key: "{key}",
                    role: "tab",
                    class: if showing() == key { "tab tab-active" } else { "tab" },
                    onclick: move |_| showing.set(key.to_string()),
                    "{name}"
                }
            }
        }

        // Keyed on the scope and the file so switching either remounts the
        // editor. Without that, the draft signal would survive the change:
        // one file's edits under the other's name, or one package's file
        // read — and written — as the package left behind.
        for (key, name) in FILES {
            if showing() == key {
                ConfigFileEditor {
                    key: "{pkgbase.as_deref().unwrap_or(\"global\")}-{key}",
                    setting: key,
                    name,
                    pkgbase: pkgbase.clone(),
                }
            }
        }
    }
}

/// One file: what it currently is, and a way to change it.
///
/// `pkgbase` picks the scope. In the package scope the file is an override:
/// unset means the server-wide file applies, not the builder image's — the
/// hierarchy has one more rung, which is why the badges below say "inherited"
/// rather than naming a specific fallback they cannot see from here.
#[component]
fn ConfigFileEditor(setting: String, name: String, pkgbase: Option<String>) -> Element {
    // Props read directly, not mirrored into signals: the call site keys on
    // scope and file, so a new scope or file remounts rather than reusing a
    // draft — and a `patch_setting` can never write the package left behind.
    let scoped = pkgbase.is_some();
    let reload = use_signal(|| 0u32);
    // Its own pair: the resource closure is `FnMut`, so it clones per run
    // instead of moving the props, which the save buttons still need.
    let res_props = (setting.clone(), pkgbase.clone());
    let loaded = use_resource(move || {
        let (setting, pkgbase) = res_props.clone();
        async move {
            // Read so a save re-fetches: the server owns the value, and the
            // source badge has to follow what it actually stored.
            let _ = reload();
            crate::api::client()?
                .get_setting(pkgbase.as_deref(), &setting)
                .await
                .map_err(|e| e.to_string())
        }
    });

    use crate::setting_editor::{
        MutationOutcome, SettingEdit, SettingScope, use_editor_draft, use_setting_mutations,
    };
    let scope = SettingScope::from(pkgbase);
    let controller = use_setting_mutations();
    let busy = controller.busy;
    let status = controller.status;
    let initial = loaded
        .read()
        .as_ref()
        .and_then(|value| value.as_ref().ok())
        .map(|response| response.value.clone());
    let mut draft = use_editor_draft(initial);
    let source = use_memo(move || {
        loaded
            .read()
            .as_ref()
            .and_then(|value| value.as_ref().ok())
            .map_or(SettingSource::Default, |response| response.source)
    });
    let dirty = draft().is_dirty();
    let reset_title = if scoped {
        "Discard this package's copy and use the server-wide file"
    } else {
        "Discard the stored file and use the builder's own copy"
    };
    let setting = aurcache_client::Setting::from_key(&setting).expect("config file setting");
    let save_scope = scope.clone();
    let save = move |value: Option<String>| {
        let scope = save_scope.clone();
        async move {
            let edit = value
                .map_or_else(
                    || SettingEdit::reset(setting),
                    |value| SettingEdit::set(setting, value),
                )
                .editing(draft);
            if controller.apply(&scope, edit).await == MutationOutcome::Applied {
                let mut reload = reload;
                reload += 1;
            }
        }
    };

    rsx! {
        div { class: "pt-3 space-y-2",
            match &*loaded.read_unchecked() {
                None => rsx! {
                    div { class: "flex justify-center p-8",
                        span { class: "loading loading-spinner loading-lg" }
                    }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error", span { "Could not load {name}: {e}" } }
                },
                Some(Ok(_)) => rsx! {
                    div { class: "flex items-center gap-2 flex-wrap",
                        span { class: "font-mono text-sm", "{name}" }
                        // Says which copy is on screen. Without it an unset
                        // file and a stored one that happens to match the
                        // image's are indistinguishable.
                        if scope.is_overridden(source()) {
                            span { class: "badge badge-outline badge-sm",
                                if scoped { "package override" } else { "stored" }
                            }
                        } else {
                            span { class: "badge badge-outline badge-sm",
                                if scoped { "inherited" } else { "builder default" }
                            }
                        }
                        if dirty {
                            span { class: "badge badge-info badge-sm", "unsaved" }
                        }
                        div { class: "flex-1" }
                        if scope.is_overridden(source()) {
                            button {
                                class: "btn btn-ghost btn-sm",
                                disabled: busy(),
                                title: reset_title,
                                onclick: {
                                    // Cloned, not moved: the Save button below
                                    // needs its own.
                                    let save = save.clone();
                                    move |_| save(None)
                                },
                                "Reset"
                            }
                        }
                        button {
                            class: "btn btn-primary btn-sm",
                            disabled: !dirty || busy(),
                            onclick: move |_| save(Some(draft().text())),
                            if busy() {
                                span { class: "loading loading-spinner loading-xs" }
                            }
                            "Save"
                        }
                    }

                    if let Some(feedback) = status() {
                        match feedback {
                            Ok(message) => rsx! {
                                div { class: "alert alert-success text-sm", span { "{message}" } }
                            },
                            Err(message) => rsx! {
                                div { class: "alert alert-error text-sm", span { "{message}" } }
                            },
                        }
                    }

                    textarea {
                        class: "textarea textarea-bordered font-mono text-xs w-full h-[60vh] leading-snug",
                        spellcheck: "false",
                        value: draft().text(),
                        oninput: move |e| draft.with_mut(|draft| draft.edit(e.value())),
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FILES;
    use aurcache_client::Setting;

    /// Every file here must be a setting the server knows, or saving it is a
    /// 404 that only shows up by clicking Save.
    #[test]
    fn each_file_is_a_setting_the_server_accepts() {
        for (key, _) in FILES {
            assert!(
                Setting::from_key(key).is_some(),
                "{key} is not a setting key"
            );
        }
    }

    /// These two are settings with no environment variable, which is why this
    /// page offers only stored-or-default and no "unset $VAR" note.
    #[test]
    fn neither_file_can_be_pinned_from_the_environment() {
        for (key, _) in FILES {
            let setting = Setting::from_key(key).expect("a known setting");
            assert_eq!(
                setting.meta().env_name,
                None,
                "{key} gained an env var; this page needs to handle it"
            );
        }
    }
}
