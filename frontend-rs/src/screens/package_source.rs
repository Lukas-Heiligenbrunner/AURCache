//! Editing a package's source files.

use crate::routes::Route;
use crate::widgets::{ConfirmDialog, Consequence, Modal};
use aurcache_client::SourceFileContent;
use dioxus::prelude::*;

/// The screen behind `/package/:pkgbase/source/:..path`.
///
/// The path arrives as segments because a source path contains slashes;
/// rejoining them is what makes a nested file linkable.
#[component]
pub fn PackageSource(pkgbase: String, path: Vec<String>) -> Element {
    let initial_path = (!path.is_empty()).then(|| path.join("/"));
    rsx! { SourceEditor { pkgbase, initial_path } }
}

// ---------------------------------------------------------------------------
// Source patch editor
//
// The server stores a package's local modifications as a diff against the
// pristine upstream source. The editor never shows that diff: it shows a file,
// you change it, and the server derives the patch. Writing the original
// content back is how a file is un-patched.
//
// No syntax highlighting: a textarea needs no JS editor component.
// ---------------------------------------------------------------------------

/// The text the editor shows and measures the draft against: the patched
/// content when there is some, otherwise the pristine file. One copy — this
/// selection is wherever the draft is seeded, dirtied, compared or reverted.
fn shown_content(content: &SourceFileContent) -> &str {
    content
        .patched_content
        .as_deref()
        .unwrap_or(&content.original_content)
}

/// What the open file's draft is, measured against what it was opened from.
/// All false while no file is open.
#[derive(Clone, Copy, Default)]
struct FileState {
    /// The draft differs from what the editor opened with.
    edited: bool,
    /// The file carries a patch that applied.
    patched: bool,
    /// The stored patch no longer applies.
    broken: bool,
    /// The draft differs from the pristine upstream file.
    differs_from_upstream: bool,
    /// A patch is stored for the file, applying or not.
    has_patch: bool,
}

#[component]
pub fn SourceEditor(pkgbase: String, initial_path: Option<String>) -> Element {
    // `use_reactive` so the fetch follows the route, as the scope's own does.
    let files = use_resource(use_reactive(&pkgbase, |pkgbase| async move {
        let client = crate::api::client()?;
        client
            .list_source_files(&pkgbase)
            .await
            .map_err(|e| e.to_string())
    }));

    let editor = crate::source_editor::use_file_editor();
    let selected = editor.selected;
    let loaded = editor.loaded;
    let mut draft = editor.draft;
    let mut error = editor.error;
    let mut show_patch = use_signal(|| false);
    let mut patch_copied = use_signal(|| false);
    let mut patch_copy_error = use_signal(|| Option::<String>::None);
    // The Reset menu is open. Closed again whenever another file is opened.
    let mut reset_open = use_signal(|| false);
    // A file asked for while the open one has unsaved edits: opening it would
    // throw them away, so it waits here for "Discard and continue".
    let mut pending_file = use_signal(|| Option::<String>::None);

    let open_file = move |pkgbase: String, path: String| async move {
        reset_open.set(false);
        show_patch.set(false);
        editor
            .open(
                crate::source_editor::SourceLocation::Package(pkgbase),
                path,
                None,
            )
            .await;
    };

    // Open the file named in the URL, if any, and reset when the page
    // changes. Navigating between source pages reuses this component, so the
    // previous file's selection must not suppress the file the URL names --
    // and the previous file's content must not linger behind the new one.
    // `use_reactive` like the fetches above, so this follows the route.
    use_effect(use_reactive(
        (&pkgbase, &initial_path),
        move |(pkgbase, initial_path)| {
            editor.reset();
            pending_file.set(None);
            show_patch.set(false);
            reset_open.set(false);
            if let Some(path) = initial_path {
                spawn(async move { open_file(pkgbase, path).await });
            }
        },
    ));

    let state = match loaded.read().as_ref() {
        Some(c) => FileState {
            edited: draft() != shown_content(c),
            patched: c.patched_content.is_some(),
            broken: c.patch_error.is_some(),
            differs_from_upstream: draft() != c.original_content,
            has_patch: c.stored_patch.is_some(),
        },
        None => FileState::default(),
    };
    let FileState {
        edited,
        patched,
        broken,
        differs_from_upstream: can_revert_to_upstream,
        has_patch,
    } = state;
    let can_save = (edited || broken) && !(editor.loading)();
    // Two ways back, because there are two things an edit can be measured
    // against. The saved version is what the editor opened with: the patched
    // file, or upstream when there is no patch or it no longer applies. Upstream
    // drops every local change, saved or not, and a save then removes the
    // patch.
    let can_revert_to_saved = edited;

    // The Save & Rebuild handler takes `pkgbase` itself; the discard dialog,
    // rendered after it, needs its own copy.
    let pkgbase_for_discard = pkgbase.clone();

    rsx! {
        crate::source_editor::SourcePane {
            title: pkgbase.clone(),
            files: match &*files.read_unchecked() {
                None => None,
                Some(Ok(list)) => Some(Ok(list.files.clone())),
                Some(Err(e)) => Some(Err(e.clone())),
            },
            patched_files: match &*files.read_unchecked() {
                Some(Ok(list)) => list.patched.clone(),
                _ => Vec::new(),
            },
            selected: selected(),
            onselect: {
                let pkgbase = pkgbase.clone();
                move |path: String| {
                    if selected.peek().as_deref() == Some(path.as_str()) {
                        return;
                    }
                    // Read at click time, not captured from the render: the
                    // draft changes with every keystroke.
                    let unsaved = loaded
                        .peek()
                        .as_ref()
                        .is_some_and(|c| *draft.peek() != shown_content(c));
                    if unsaved {
                        pending_file.set(Some(path));
                        return;
                    }
                    let pkgbase = pkgbase.clone();
                    spawn(async move { open_file(pkgbase, path).await });
                }
            },
            draft,
            loading: (editor.loading)(),
            dirty: edited,
            patched,
            patch_failed: broken,
            actions: rsx! {
                // Whenever the file carries a patch, not only a broken one:
                // reading the diff is the quickest way to see what a package
                // changes about its upstream, and whether that is still wanted.
                if has_patch {
                    button {
                        class: "btn btn-ghost btn-sm",
                        onclick: move |_| show_patch.set(true),
                        "View patch"
                    }
                }
                div { class: "relative",
                    button {
                        class: "btn btn-ghost btn-sm",
                        disabled: !(can_revert_to_saved || can_revert_to_upstream),
                        aria_haspopup: "menu",
                        aria_expanded: "{reset_open}",
                        onclick: move |_| reset_open.toggle(),
                        "Reset ▾"
                    }
                    if reset_open() {
                        // Anywhere else on the page closes the menu without
                        // choosing, as clicking away from a menu does.
                        button {
                            class: "fixed inset-0 z-10 cursor-default",
                            tabindex: "-1",
                            aria_label: "Close the reset menu",
                            onclick: move |_| reset_open.set(false),
                        }
                        ul {
                            class: "menu menu-sm absolute right-0 top-full mt-1 w-72 z-20 bg-base-200 rounded-box shadow-lg",
                            role: "menu",
                            li { class: if !can_revert_to_saved { "disabled" },
                                button {
                                    role: "menuitem",
                                    disabled: !can_revert_to_saved,
                                    onclick: move |_| {
                                        reset_open.set(false);
                                        if let Some(c) = loaded.read().as_ref() {
                                            draft.set(shown_content(c).to_owned());
                                        }
                                    },
                                    span { class: "flex flex-col items-start",
                                        span { "Revert to saved version" }
                                        span { class: "text-xs opacity-60", "Discard the edits made since opening it" }
                                    }
                                }
                            }
                            li { class: if !can_revert_to_upstream { "disabled" },
                                button {
                                    role: "menuitem",
                                    disabled: !can_revert_to_upstream,
                                    onclick: move |_| {
                                        reset_open.set(false);
                                        if let Some(c) = loaded.read().as_ref() {
                                            draft.set(c.original_content.clone());
                                        }
                                    },
                                    span { class: "flex flex-col items-start",
                                        span { "Revert to upstream" }
                                        span { class: "text-xs opacity-60",
                                            "Drop every local change; saving then removes the patch"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                button {
                    class: "btn btn-sm",
                    disabled: !can_save,
                    onclick: {
                        let pkgbase = pkgbase.clone();
                        move |_| {
                            let pkgbase = pkgbase.clone();
                            async move {
                                let Some(path) = selected() else { return };
                                let saved = async {
                                    crate::api::client()?
                                        .put_source_file(&pkgbase, &path, &draft())
                                        .await
                                        .map_err(|e| e.to_string())
                                }
                                .await;
                                match saved {
                                    Ok(()) => {
                                        navigator().push(Route::Package { pkgbase: pkgbase.clone() });
                                    }
                                    Err(e) => error.set(Some(e)),
                                }
                            }
                        }
                    },
                    "Save"
                }
                button {
                    class: "btn btn-primary btn-sm",
                    disabled: !can_save,
                    // Editing a PKGBUILD is nearly always a prelude to building
                    // it; without this the next step is a save, a navigation
                    // back, and a second button.
                    onclick: {
                        move |_| {
                            let pkgbase = pkgbase.clone();
                            async move {
                                let Some(path) = selected() else { return };
                                let client = match crate::api::client() {
                                    Ok(client) => client,
                                    Err(e) => {
                                        error.set(Some(e));
                                        return;
                                    }
                                };
                                // The rebuild is only queued if the save
                                // worked: rebuilding the old source would
                                // report success for a change never stored.
                                match client.put_source_file(&pkgbase, &path, &draft()).await {
                                    Err(e) => error.set(Some(e.to_string())),
                                    Ok(()) => match client
                                        .update_package(&pkgbase, &aurcache_client::UpdatePackage { force: true })
                                        .await
                                    {
                                        // To the build just queued, not back to
                                        // the package: the build page polls
                                        // while the build runs, so this is the
                                        // edit being watched rather than the
                                        // edit disappearing behind a package
                                        // header. An update queues one build
                                        // per platform, and the page for the
                                        // first of them is where the action
                                        // sits.
                                        Ok(numbers) => match numbers.first() {
                                            Some(&number) => {
                                                navigator().push(Route::Build {
                                                    pkgbase: pkgbase.clone(),
                                                    number,
                                                });
                                            }
                                            // A forced update queues something,
                                            // but not dying to a blank build
                                            // page if it ever queues nothing.
                                            None => {
                                                navigator().push(Route::Package { pkgbase: pkgbase.clone() });
                                            }
                                        },
                                        // Stay put on a partial failure: the
                                        // edit is saved but the build is not
                                        // queued, and leaving would hide that.
                                        Err(e) => error.set(Some(format!(
                                            "Saved, but the rebuild could not be queued: {e}"
                                        ))),
                                    },
                                }
                            }
                        }
                    },
                    "Save & Rebuild"
                }
            },
            notices: rsx! {
                // A patch that no longer applies is shown, not hidden: the
                // editor is falling back to pristine content and the user needs
                // to know before saving over it.
                if let Some(err) = loaded.read().as_ref().and_then(|c| c.patch_error.clone()) {
                    div { class: "alert alert-warning text-sm",
                        span { "Stored patch no longer applies: {err}. Showing upstream content." }
                    }
                }
                if let Some(message) = error() {
                    div {
                        class: "alert alert-error text-sm",
                        span { "{message}" }
                    }
                }
            },
        }
        // Opening another file replaces the draft, so unsaved edits ask first.
        // "Stay" is the default and what clicking away means.
        if let Some(next) = pending_file() {
            ConfirmDialog {
                open: true,
                label: "Discard unsaved changes",
                title: "Discard changes?",
                cancel: "Stay",
                confirm: "Discard and continue",
                consequence: Consequence::Discards,
                on_cancel: move |()| pending_file.set(None),
                on_confirm: {
                    let pkgbase = pkgbase_for_discard;
                    move |()| {
                        let Some(path) = pending_file.take() else { return };
                        let pkgbase = pkgbase.clone();
                        spawn(async move { open_file(pkgbase, path).await });
                    }
                },
                {format!(
                    "{} has edits that are not saved. Opening {next} discards them.",
                    selected().unwrap_or_default()
                )}
            }
        }
        if show_patch() {
            if let Some(c) = loaded.read().as_ref()
                && let Some(patch) = c.stored_patch.clone()
            {
                Modal {
                    open: true,
                    label: "Stored patch",
                    on_close: move |()| {
                        show_patch.set(false);
                        patch_copy_error.set(None);
                    },
                    class: "max-w-3xl",
                    h3 { class: "font-bold text-lg", "Stored patch" }
                    p { class: "text-xs opacity-60 font-mono break-all", "{c.path}" }
                    if let Some(err) = patch_copy_error() {
                        div { class: "alert alert-error text-sm mt-2",
                            span { "{err}" }
                        }
                    }
                    pre { class: "max-h-[60vh] overflow-auto text-xs font-mono mt-3",
                        for (index, line) in patch.lines().enumerate() {
                            div {
                                key: "{index}",
                                class: "whitespace-pre-wrap {DiffLineKind::of(line).classes()}",
                                "{line}"
                            }
                        }
                    }
                    div { class: "modal-action",
                        {
                            let patch_for_copy = patch;
                            rsx! {
                                button {
                                    class: "btn btn-sm",
                                    disabled: patch_copied(),
                                    onclick: move |_| {
                                        let patch = patch_for_copy.clone();
                                        async move {
                                            match crate::clipboard::copy_text(&patch).await {
                                                Ok(()) => {
                                                    patch_copied.set(true);
                                                    patch_copy_error.set(None);
                                                    gloo_timers::future::TimeoutFuture::new(2000).await;
                                                    patch_copied.set(false);
                                                }
                                                Err(e) => patch_copy_error.set(Some(e.message("the patch"))),
                                            }
                                        }
                                    },
                                    if patch_copied() { "Copied" } else { "Copy" }
                                }
                            }
                        }
                        button {
                            class: "btn btn-sm",
                            onclick: move |_| {
                                show_patch.set(false);
                                patch_copy_error.set(None);
                            },
                            "Close"
                        }
                    }
                }
            }
        }
    }
}

/// How to classify a line of a unified diff for display, git-style.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DiffLineKind {
    Context,
    Added,
    Removed,
    Hunk,
}

impl DiffLineKind {
    fn of(line: &str) -> Self {
        if line.starts_with("@@") || line.starts_with("---") || line.starts_with("+++") {
            Self::Hunk
        } else if line.starts_with('+') {
            Self::Added
        } else if line.starts_with('-') {
            Self::Removed
        } else {
            Self::Context
        }
    }

    fn classes(self) -> &'static str {
        match self {
            Self::Context => "",
            Self::Added => "text-success",
            Self::Removed => "text-error",
            Self::Hunk => "text-info",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DiffLineKind;

    #[test]
    fn classifies_unified_diff_lines() {
        assert_eq!(DiffLineKind::of("@@ -1,3 +1,4 @@"), DiffLineKind::Hunk);
        assert_eq!(DiffLineKind::of("--- a/PKGBUILD"), DiffLineKind::Hunk);
        assert_eq!(DiffLineKind::of("+++ b/PKGBUILD"), DiffLineKind::Hunk);
        assert_eq!(DiffLineKind::of("+pkgrel=2"), DiffLineKind::Added);
        assert_eq!(DiffLineKind::of("-pkgrel=1"), DiffLineKind::Removed);
        assert_eq!(DiffLineKind::of(" context"), DiffLineKind::Context);
        assert_eq!(DiffLineKind::of(""), DiffLineKind::Context);
    }
}
