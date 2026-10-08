//! Shared setting mutations and draft reseeding rules.
use aurcache_client::{Setting, SettingSource};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingScope {
    Global,
    Package(String),
}
impl From<Option<String>> for SettingScope {
    fn from(package: Option<String>) -> Self {
        package.map_or(Self::Global, Self::Package)
    }
}
impl SettingScope {
    pub fn package(&self) -> Option<&str> {
        match self {
            Self::Global => None,
            Self::Package(package) => Some(package),
        }
    }
    pub fn is_overridden(&self, source: SettingSource) -> bool {
        match self {
            Self::Global => source == SettingSource::Global,
            Self::Package(_) => source == SettingSource::Package,
        }
    }
}

#[derive(Clone, Debug)]
pub enum SettingChange {
    Set(String),
    Reset,
}

#[derive(Clone)]
pub struct SettingEdit {
    pub setting: Setting,
    pub change: SettingChange,
    pub draft: Option<Signal<EditorDraft>>,
}
impl SettingEdit {
    pub fn set(setting: Setting, value: String) -> Self {
        Self {
            setting,
            change: SettingChange::Set(value),
            draft: None,
        }
    }
    pub fn reset(setting: Setting) -> Self {
        Self {
            setting,
            change: SettingChange::Reset,
            draft: None,
        }
    }
    pub fn editing(mut self, draft: Signal<EditorDraft>) -> Self {
        self.draft = Some(draft);
        self
    }
}

/// The editable value and the server value against which it is dirty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorDraft {
    pub value: Option<String>,
    stored: Option<String>,
    reset_revision: u64,
}
impl EditorDraft {
    pub fn text(&self) -> String {
        self.value.clone().unwrap_or_default()
    }
    pub fn is_dirty(&self) -> bool {
        self.value != self.stored
    }
    pub fn edit(&mut self, value: String) {
        self.value = Some(value);
    }
    pub fn seed(&mut self, value: String) {
        if !self.is_dirty() {
            self.value = Some(value.clone());
            self.stored = Some(value);
        }
    }
    fn accept(&mut self, change: &SettingChange, submitted: &Option<String>) {
        match change {
            SettingChange::Set(value) => self.stored = Some(value.clone()),
            SettingChange::Reset => {
                self.stored.clone_from(submitted);
                // A reset can leave the effective server value unchanged.
                // Reseed even then, while preserving edits made in flight.
                self.reset_revision = self.reset_revision.wrapping_add(1);
            }
        }
    }
}

pub fn use_editor_draft(loaded: Option<String>) -> Signal<EditorDraft> {
    let mut draft = use_signal(EditorDraft::default);
    let reset_revision = draft.read().reset_revision;
    use_effect(use_reactive(
        &(loaded, reset_revision),
        move |(loaded, _)| {
            if let Some(value) = loaded {
                draft.with_mut(|draft| draft.seed(value));
            }
        },
    ));
    draft
}

#[derive(Clone, Copy)]
pub struct SettingMutationController {
    pub busy: Signal<bool>,
    pub status: Signal<Option<Result<String, String>>>,
}
#[derive(PartialEq, Eq)]
pub enum MutationOutcome {
    Applied,
    Failed,
    Busy,
}

pub fn use_setting_mutations() -> SettingMutationController {
    SettingMutationController {
        busy: use_signal(|| false),
        status: use_signal(|| None),
    }
}

impl SettingMutationController {
    /// Apply one setting change, preserving edits made during the request.
    pub async fn apply(mut self, scope: &SettingScope, edit: SettingEdit) -> MutationOutcome {
        if *self.busy.peek() {
            return MutationOutcome::Busy;
        }
        self.busy.set(true);
        self.status.set(None);
        let submitted = edit.draft.and_then(|draft| draft.peek().value.clone());
        let outcome = match crate::api::client() {
            Err(error) => Err(error),
            Ok(client) => match &edit.change {
                SettingChange::Set(value) => {
                    client
                        .patch_setting(scope.package(), edit.setting.meta().key, value)
                        .await
                }
                SettingChange::Reset => {
                    client
                        .reset_setting(scope.package(), edit.setting.meta().key)
                        .await
                }
            }
            .map_err(|error| error.to_string()),
        };
        self.busy.set(false);
        match outcome {
            Ok(()) => {
                if let Some(mut draft) = edit.draft {
                    draft.with_mut(|draft| draft.accept(&edit.change, &submitted));
                }
                let message = match (&edit.change, scope) {
                    (SettingChange::Set(_), _) => "Saved.",
                    (SettingChange::Reset, SettingScope::Global) => "Reset to the default.",
                    (SettingChange::Reset, SettingScope::Package(_)) => {
                        "Reset to the inherited value."
                    }
                };
                self.status.set(Some(Ok(message.to_owned())));
                MutationOutcome::Applied
            }
            Err(error) => {
                self.status.set(Some(Err(error)));
                MutationOutcome::Failed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delayed_load_and_refetch_preserve_edits() {
        let mut draft = EditorDraft::default();
        draft.edit("typed early".into());
        draft.seed("loaded later".into());
        assert_eq!(draft.text(), "typed early");
        draft.accept(
            &SettingChange::Set("typed early".into()),
            &draft.value.clone(),
        );
        draft.seed("normalised".into());
        assert_eq!(draft.text(), "normalised");
    }
    #[test]
    fn edits_during_save_and_reset_stay_dirty() {
        let mut draft = EditorDraft::default();
        draft.seed("old".into());
        draft.edit("submitted".into());
        let submitted = draft.value.clone();
        draft.edit("new edit".into());
        draft.accept(&SettingChange::Set("submitted".into()), &submitted);
        assert!(draft.is_dirty());
        draft.accept(&SettingChange::Reset, &submitted);
        draft.seed("inherited".into());
        assert_eq!(draft.text(), "new edit");
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod reset_tests {
        use super::*;

        thread_local! {
            static RESET_DRAFT: std::cell::RefCell<Option<Signal<EditorDraft>>> = const {
                std::cell::RefCell::new(None)
            };
        }

        fn reset_harness() -> Element {
            let draft = use_editor_draft(Some("inherited".into()));
            RESET_DRAFT.with(|slot| *slot.borrow_mut() = Some(draft));
            rsx! { span { "{draft().text()}" } }
        }

        async fn flush_effects(dom: &mut VirtualDom) {
            for _ in 0..5 {
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(2), dom.wait_for_work())
                        .await;
                dom.render_immediate(&mut dioxus_core::NoOpMutations);
            }
        }

        #[tokio::test]
        async fn reset_reseeds_an_unchanged_inherited_value_and_preserves_new_edits() {
            let mut dom = VirtualDom::new(reset_harness);
            dom.rebuild_in_place();
            flush_effects(&mut dom).await;
            let mut draft = RESET_DRAFT.with(|slot| slot.borrow().unwrap());
            assert_eq!(draft.peek().text(), "inherited");

            dom.in_runtime(|| draft.with_mut(|draft| draft.edit("unsaved edit".into())));
            flush_effects(&mut dom).await;
            dom.in_runtime(|| {
                draft.with_mut(|draft| {
                    let submitted = draft.value.clone();
                    draft.accept(&SettingChange::Reset, &submitted);
                });
            });
            flush_effects(&mut dom).await;
            assert_eq!(draft.peek().text(), "inherited");
            assert!(!draft.peek().is_dirty());

            dom.in_runtime(|| {
                draft.with_mut(|draft| {
                    draft.edit("submitted".into());
                    let submitted = draft.value.clone();
                    draft.edit("typed during reset".into());
                    draft.accept(&SettingChange::Reset, &submitted);
                });
            });
            flush_effects(&mut dom).await;
            assert_eq!(draft.peek().text(), "typed during reset");
            assert!(draft.peek().is_dirty());
        }
    }
}
