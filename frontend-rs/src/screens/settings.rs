//! Server-wide settings: what they are, what they resolve to, and where each
//! value came from.
//!
//! The value alone is not enough to act on. A number shown without its origin
//! looks editable even when an environment variable is pinning it, and looks
//! stored even when it is only the built-in default — so every row says which
//! of the two it is, and offers a reset only when there is something stored to
//! reset.

use crate::dates::{Clock, DateOrder, DateParts, DateStyle, render};
use crate::listing::ListHeader;
use crate::routes::Route;
use crate::setting_editor::{
    EditorDraft, MutationOutcome, SettingEdit, SettingScope, use_editor_draft,
    use_setting_mutations,
};
use aurcache_client::{ApplicationSettings, SchedulePreview, Setting, SettingSource, Timezone};
use dioxus::prelude::*;

#[component]
pub fn Settings() -> Element {
    let mut settings = use_resource(|| async move {
        crate::api::client()?
            .settings(None)
            .await
            .map_err(|e| e.to_string())
    });

    // Which timezone the auto-update schedule is read in. A failure leaves the
    // row saying nothing about it, which is what it said before servers
    // reported one.
    let server_timezone = use_resource(|| async move {
        let client = crate::api::client().ok()?;
        client.server_info().await.ok()?.timezone
    });

    let controller = use_setting_mutations();
    let status = controller.status;
    let save = move |edit: SettingEdit| async move {
        if controller.apply(&SettingScope::Global, edit).await == MutationOutcome::Applied {
            settings.restart();
        }
    };

    rsx! {
        div { class: "space-y-4",
            ListHeader { title: "Settings" }

            if let Some(feedback) = status() {
                div {
                    class: if feedback.is_ok() { "alert alert-success text-sm" } else { "alert alert-error text-sm" },
                    span { "{feedback.as_ref().unwrap_or_else(|error| error)}" }
                }
            }

            match &*settings.read_unchecked() {
                None => rsx! { span { class: "loading loading-spinner" } },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error", span { "Could not load settings: {e}" } }
                },
                Some(Ok(loaded)) => rsx! {
                    fieldset { disabled: (controller.busy)(),
                    SettingsSections {
                        settings: loaded.clone(),
                        schedule_zone: schedule_zone(
                            server_timezone.read().clone().flatten().as_ref(),
                            &crate::dates::viewer_timezone(),
                        ),
                        save,
                    }
                    }
                },
            }
        }
    }
}

#[component]
fn SettingsSections(
    settings: ApplicationSettings,
    schedule_zone: ScheduleZone,
    save: EventHandler<SettingEdit>,
) -> Element {
    rsx! {
        // Two columns once there is genuinely room. A single column of settings
        // on a 1920-wide screen leaves each row a label at one edge and a
        // control at the other with a metre of nothing between — but splitting
        // earlier than `2xl` makes each column too narrow for a label and a
        // control side by side, which is worse than the stretch.
        //
        // Columns, not a grid: the sections are of very different heights, and
        // a two-column grid would pad every short card out to match the tall
        // one beside it.
        div { class: "2xl:columns-2 2xl:gap-4",
        Section { title: "General",
            SettingRow {
                setting: Setting::VersionCheckInterval,
                label: "Version check interval",
                description: "How often to look for new AUR and git versions, such as 1h or 30m.",
                // Written back as it is typed, like the size below.
                value: aurcache_common::units::format_duration(u64::from(settings.version_check_interval.value)),
                source: settings.version_check_interval.source,
                editor: Editor::Text { placeholder: "1h".to_string(), wide: false },
                save,
            }
            SettingRow {
                setting: Setting::AutoUpdateSchedule,
                label: "Auto-update schedule",
                description: schedule_zone.description,
                warning: schedule_zone.warning,
                // Distinct from a stored empty string, which is what
                // "disabled" is: the field renders empty either way, and the
                // source badge is what tells the two apart.
                value: settings.auto_update_schedule.value.clone().unwrap_or_default(),
                source: settings.auto_update_schedule.source,
                editor: Editor::Schedule { placeholder: "@daily".to_string() },
                save,
            }
            SettingRow {
                setting: Setting::BuildOnNewVersion,
                label: "Build on new version",
                description: "Queue a rebuild the moment a new version is detected, without waiting for the schedule above.",
                value: settings.build_on_new_version.value.to_string(),
                source: settings.build_on_new_version.source,
                editor: Editor::Toggle,
                save,
            }
        }

        Section { title: "Default date format",
            DateFormatRows {
                value: settings.date_format.value.clone(),
                source: settings.date_format.source,
                save,
            }
        }

        Section { title: "Builds",
            // Laid out as the setting rows below it are, rather than as a
            // link in running text: it belongs to this section's list of
            // things you can change, and reads as one of them.
            div { class: "py-3 flex gap-3 items-center",
                div { class: "min-w-0 flex-1",
                    div { class: "font-medium text-sm", "Config files" }
                    p { class: "text-xs opacity-60 max-w-prose",
                        "The makepkg.conf and pacman.conf every build uses."
                    }
                }
                Link { class: "btn btn-sm btn-outline", to: Route::ConfigFiles {}, "Edit" }
            }
            SettingRow {
                setting: Setting::JobTimeout,
                label: "Job timeout",
                description: "How long a single build may run before the server abandons it, such as 3h.",
                value: aurcache_common::units::format_duration(u64::from(settings.job_timeout.value)),
                source: settings.job_timeout.source,
                editor: Editor::Text { placeholder: "1h".to_string(), wide: false },
                save,
            }
            SettingRow {
                setting: Setting::MaxArtifactSize,
                label: "Max artifact size",
                description: "Largest package file a worker may upload, such as 20G or 512M. A package's own setting overrides it.",
                // Written back in the form it is typed in, so an untouched
                // field saves the same value rather than a byte count.
                value: aurcache_common::units::format_size(settings.max_artifact_size.value),
                source: settings.max_artifact_size.source,
                editor: Editor::Text { placeholder: "20G".to_string(), wide: false },
                save,
            }
        }

        ApiAccessSection {}
        crate::screens::backup::BackupSection {}
        }
    }
}

#[component]
pub fn Section(title: String, children: Element) -> Element {
    rsx! {
        // `break-inside-avoid` and a margin rather than a gap: the sections are
        // laid out in CSS columns (see `SettingsSections`), which flow rather
        // than grid, so a card must be told not to be split across the fold.
        div { class: "card bg-base-100 shadow-xl break-inside-avoid mb-4",
            div { class: "card-body",
                h2 { class: "card-title text-base", "{title}" }
                div { class: "divide-y divide-base-300", {children} }
            }
        }
    }
}

/// How a setting's value is edited.
///
/// Not how it is *typed*: the server parses every value from a string and owns
/// the rules, so this only decides which control a person is given.
#[derive(Clone, PartialEq)]
enum Editor {
    /// `wide` for values that are long by nature, such as an image reference:
    /// the default box shows only the registry and hides the tag.
    Text {
        placeholder: String,
        wide: bool,
    },
    /// A schedule: a text field, with when it would run underneath, so a
    /// mistake shows while it is being typed rather than at 3 am.
    Schedule {
        placeholder: String,
    },
    Toggle,
}

impl Editor {
    /// Whether this control wants a line to itself.
    fn is_stacked(&self) -> bool {
        matches!(self, Self::Text { wide: true, .. })
    }
}

/// How a row arranges its label and its control.
fn row_layout(stacked: bool) -> &'static str {
    if stacked {
        "flex-col"
    } else {
        "flex-wrap items-start justify-between"
    }
}

/// A text field wide enough for what it holds.
///
/// The wide case is on its own line already (see [`Editor::is_stacked`]), so it
/// takes the row rather than a fixed width.
fn text_width(wide: bool) -> &'static str {
    if wide { "w-full" } else { "w-64" }
}

/// Whether this scope holds a stored value that a reset could clear.
///
/// `Default` has nothing stored, and `Env` is not stored here at all — an
/// environment variable outranks the database, so deleting the row would
/// change nothing while appearing to.
pub fn is_stored(source: SettingSource) -> bool {
    matches!(source, SettingSource::Global | SettingSource::Package)
}

/// The badge beside a setting's name.
///
/// Only `Default` gets one. `Env` says more than a badge has room for and is
/// rendered as [`EnvNote`] under the description instead; a stored value is the
/// ordinary case and needs no label.
#[component]
fn SourceNote(source: SettingSource) -> Element {
    match source {
        SettingSource::Default => rsx! {
            span { class: "badge badge-outline badge-sm whitespace-nowrap", "default" }
        },
        SettingSource::Env | SettingSource::Global | SettingSource::Package => rsx! {},
    }
}

/// A help marker naming the environment variable behind a setting.
///
/// Every row carries one, set or not: this page is where someone looks to find
/// out what a deployment *can* pin, and a name that only appears once the
/// variable is already set is documentation you cannot reach when you need it.
/// On hover rather than in the row, because eleven permanently visible variable
/// names crowd out the settings they annotate.
///
/// The name comes from the setting's own metadata rather than being written out
/// again here, so it cannot point at a variable the server does not read.
#[component]
fn EnvHelp(setting: Setting) -> Element {
    let Some(name) = setting.meta().env_name else {
        return rsx! {};
    };
    rsx! {
        span {
            class: "cursor-help text-xs opacity-40 hover:opacity-80 transition-opacity",
            title: "Set ${name} to pin this from the environment",
            "?"
        }
    }
}

/// What to do about a setting an environment variable has already taken over.
///
/// Stays visible rather than hiding behind [`EnvHelp`]: the field beside it is
/// disabled, and without this the row is a dead end. Saying *how* to take it
/// back matters more than saying it happened — the variable is in a compose
/// file or a unit file, nowhere this page can reach.
#[component]
fn EnvNote(setting: Setting, source: SettingSource) -> Element {
    if source != SettingSource::Env {
        return rsx! {};
    }
    let Some(name) = setting.meta().env_name else {
        return rsx! {};
    };
    rsx! {
        p { class: "text-xs text-warning max-w-prose",
            "unset ${name} to allow control here"
        }
    }
}

/// The parts of a row that do not depend on what is being edited: the name,
/// the source badge, the description, and the environment-variable notes.
///
/// Shared so that one facet of a date and a job timeout read as the same kind
/// of thing, which they are.
#[component]
fn RowShell(
    setting: Setting,
    label: String,
    description: String,
    #[props(default)] warning: Option<String>,
    source: SettingSource,
    /// Put the control on its own line under the label. For values too long to
    /// sit beside their name without squeezing it into a column of single
    /// words — a container image reference is the case that forced this.
    #[props(default = false)]
    stacked: bool,
    children: Element,
) -> Element {
    rsx! {
        div {
            class: "py-3 flex gap-3 {row_layout(stacked)}",
            div { class: "min-w-0 flex-1",
                div { class: "flex items-center gap-2 flex-wrap",
                    span { class: "font-medium text-sm", "{label}" }
                    EnvHelp { setting }
                    SourceNote { source }
                }
                if !description.is_empty() {
                    p { class: "text-xs opacity-60 max-w-prose", "{description}" }
                }
                if let Some(warning) = warning {
                    p { class: "text-xs text-warning max-w-prose", "{warning}" }
                }
                EnvNote { setting, source }
            }
            div { class: "flex items-center gap-2 min-w-0", {children} }
        }
    }
}

#[component]
fn SettingRow(
    setting: Setting,
    label: String,
    description: String,
    /// Shown under the description, for a value that means something other
    /// than the viewer would assume.
    #[props(default)]
    warning: Option<String>,
    value: String,
    source: SettingSource,
    editor: Editor,
    save: EventHandler<SettingEdit>,
) -> Element {
    // An environment variable wins over anything stored, so an editable field
    // would accept a change that has no effect.
    let locked = source == SettingSource::Env;
    let draft = use_editor_draft(Some(value.clone()));
    let dirty = draft().is_dirty();

    rsx! {
        RowShell { setting, label, description, warning, source, stacked: editor.is_stacked(),
            {
                let control = match editor {
                    Editor::Toggle => rsx! {
                        input {
                            r#type: "checkbox",
                            class: "toggle toggle-primary",
                            disabled: locked,
                            checked: draft().value.as_deref().map_or(value == "true", |value| value == "true"),
                            onchange: move |e: FormEvent| {
                                let value = e.checked().to_string();
                                let mut draft = draft;
                                draft.with_mut(|draft| draft.edit(value.clone()));
                                save.call(SettingEdit::set(setting, value).editing(draft));
                            },
                        }
                    },
                    Editor::Text { placeholder, wide } => rsx! {
                        TextField { setting, draft, placeholder, wide, locked, save }
                    },
                    Editor::Schedule { placeholder } => rsx! {
                        div { class: "flex flex-col gap-1",
                            TextField { setting, draft, placeholder, wide: false, locked, save }
                            ScheduleRuns { setting, value: draft().text() }
                        }
                    },
                };
                rsx! {
                    {control}
                    // Only shown once there is something to save. A
                    // permanently visible Save button on every row reads as a
                    // page full of unsaved changes.
                    if dirty && !locked {
                        button {
                            class: "btn btn-primary btn-sm",
                            onclick: move |_| save.call(SettingEdit::set(setting, draft().text()).editing(draft)),
                            "Save"
                        }
                    }
                    ResetSlot { setting, source, save, draft: Some(draft) }
                }
            }
        }
    }
}

/// A one-line text control that saves on Enter.
#[component]
fn TextField(
    setting: Setting,
    draft: Signal<EditorDraft>,
    placeholder: String,
    wide: bool,
    locked: bool,
    save: EventHandler<SettingEdit>,
) -> Element {
    rsx! {
        input {
            r#type: "text",
            class: "input input-bordered input-sm max-w-full font-mono text-xs {text_width(wide)}",
            disabled: locked,
            placeholder: "{placeholder}",
            value: draft().text(),
            oninput: move |e| draft.with_mut(|draft| draft.edit(e.value())),
            onkeydown: move |e: KeyboardEvent| {
                if e.key() == Key::Enter {
                    save.call(SettingEdit::set(setting, draft().text()).editing(draft));
                }
            },
        }
    }
}

/// When the schedule being typed would run, as the server works it out: only
/// it knows its own timezone and what `H` resolves to on this instance. Asked
/// again on every edit; the answer is a parse and a few date steps.
#[component]
fn ScheduleRuns(setting: Setting, value: String) -> Element {
    let style = crate::dates::use_date_style();
    let preview = use_resource(use_reactive(&value, move |value| async move {
        crate::api::client()
            .ok()?
            .preview_schedule(setting.meta().key, &value)
            .await
            .ok()
    }));
    // A failed request says nothing, rather than calling a valid schedule
    // invalid; the save is what reports a server that cannot be reached.
    let Some(Some(answer)) = preview() else {
        return rsx! {};
    };
    let note = schedule_note(&answer, |ts| crate::dates::absolute(Some(ts), style()));
    rsx! {
        p { class: if note.invalid { "text-xs text-error max-w-xs" } else { "text-xs opacity-60 max-w-xs" },
            "{note.text}"
        }
    }
}

/// What [`ScheduleRuns`] says.
#[derive(Debug, PartialEq, Eq)]
struct ScheduleNote {
    text: String,
    /// Shown as an error: the value would be refused on save.
    invalid: bool,
}

/// [`ScheduleRuns`]'s line for an answer, with `date` rendering a timestamp.
/// The runs are in the viewer's own time, which is the one they can check
/// against; the expression's hours are the server's, as the description says.
fn schedule_note(preview: &SchedulePreview, date: impl Fn(i64) -> String) -> ScheduleNote {
    let (text, invalid) = match preview {
        SchedulePreview::Runs { at } => {
            let runs: Vec<String> = at.iter().map(|&ts| date(ts)).collect();
            (
                format!("Next runs, your time: {}", runs.join(", then ")),
                false,
            )
        }
        SchedulePreview::Never => ("No date ever matches this schedule.".to_string(), false),
        SchedulePreview::Disabled => ("Off: nothing is rebuilt on a schedule.".to_string(), false),
        SchedulePreview::Invalid { reason } => (reason.clone(), true),
    };
    ScheduleNote { text, invalid }
}

/// The Reset button, in a slot that is held open whether or not this row has
/// anything to reset — otherwise one stored setting knocks its own field out of
/// the column every other field lines up in.
#[component]
fn ResetSlot(
    setting: Setting,
    source: SettingSource,
    save: EventHandler<SettingEdit>,
    #[props(default)] draft: Option<Signal<EditorDraft>>,
) -> Element {
    rsx! {
        div { class: "w-16 shrink-0 flex justify-end",
            if is_stored(source) {
                button {
                    class: "btn btn-ghost btn-sm",
                    title: "Discard the stored value and go back to the default",
                    onclick: move |_| save.call(SettingEdit { setting, change: crate::setting_editor::SettingChange::Reset, draft }),
                    "Reset"
                }
            }
        }
    }
}

/// The server-wide date default, as three settings rather than one.
///
/// They are stored as a single string — `ymd-pad-24` — but nobody thinks in
/// composite ids: the field order, the clock and the zero padding are three
/// independent choices, and presenting them as one control with three knobs
/// inside made them look like a sub-menu of something else. Each row writes the
/// whole id back with its own facet replaced.
///
/// This is the *default*: what someone sees before they choose. A browser that
/// has already chosen keeps its choice, which is what the note says — changing
/// this and seeing nothing happen is otherwise indistinguishable from a bug.
#[component]
fn DateFormatRows(
    value: String,
    source: SettingSource,
    save: EventHandler<SettingEdit>,
) -> Element {
    let locked = source == SettingSource::Env;
    let style = DateStyle::from_id(&value);
    // One reset for the three rows, because there is one stored value behind
    // them. Three would each undo all three.
    let resettable = is_stored(source);

    // Rendered against a fixed instant so the preview reads as an example
    // rather than as a pattern.
    let sample = DateParts {
        year: 2026,
        month: 8,
        day: 6,
        hour: 14,
        minute: 5,
    };

    rsx! {
        RowShell {
            setting: Setting::DateFormat,
            label: "Date order",
            description: "",
            source,
            select {
                class: "select select-bordered select-sm w-48",
                disabled: locked,
                onchange: move |e: FormEvent| {
                    let order = DateOrder::ALL
                        .into_iter()
                        .find(|o| o.id() == e.value())
                        .unwrap_or(DateOrder::Ymd);
                    save.call(SettingEdit::set(Setting::DateFormat, DateStyle { order, ..style }.id()));
                },
                for order in DateOrder::ALL {
                    option {
                        key: "{order.id()}",
                        value: order.id(),
                        selected: style.order == order,
                        "{order.label()}"
                    }
                }
            }
        }

        RowShell {
            setting: Setting::DateFormat,
            label: "Time",
            description: "",
            source,
            select {
                class: "select select-bordered select-sm w-48",
                disabled: locked,
                onchange: move |e: FormEvent| {
                    let clock = Clock::ALL
                        .into_iter()
                        .find(|c| c.id() == e.value())
                        .unwrap_or(Clock::H24);
                    save.call(SettingEdit::set(Setting::DateFormat, DateStyle { clock, ..style }.id()));
                },
                for clock in Clock::ALL {
                    option {
                        key: "{clock.id()}",
                        value: clock.id(),
                        selected: style.clock == clock,
                        "{clock.label()}"
                    }
                }
            }
        }

        RowShell {
            setting: Setting::DateFormat,
            label: "Leading zeros",
            description: "Pad the day and month to two digits.",
            source,
            input {
                r#type: "checkbox",
                class: "toggle toggle-primary",
                disabled: locked,
                checked: style.pad,
                onchange: move |e: FormEvent| {
                    let pad = e.checked();
                    save.call(SettingEdit::set(Setting::DateFormat, DateStyle { pad, ..style }.id()));
                },
            }
        }

        div { class: "py-3 flex items-center justify-between gap-3 flex-wrap",
            div { class: "min-w-0",
                p { class: "text-xs opacity-60 max-w-prose",
                    "Used for anyone who has not picked a format of their own. Your browser's choice, under Preferences, overrides this for you."
                }
                span { class: "text-xs opacity-50 font-mono", {render(sample, style)} }
            }
            if resettable && !locked {
                button {
                    class: "btn btn-ghost btn-sm",
                    title: "Discard the stored date format and go back to the default",
                    onclick: move |_| save.call(SettingEdit::reset(Setting::DateFormat)),
                    "Reset"
                }
            }
        }
    }
}

/// The personal API token, which the CLI and any script authenticate with.
#[component]
fn ApiAccessSection() -> Element {
    let user = use_resource(|| async move {
        crate::api::client()?
            .user_info()
            .await
            .map_err(|e| e.to_string())
    });

    // Held only until the page is left. The server stores a hash, so this is
    // the one and only time the token can be read.
    let mut issued = use_signal(|| Option::<String>::None);
    let mut error = use_signal(|| Option::<String>::None);

    let username = match &*user.read_unchecked() {
        Some(Ok(info)) => info.username.clone(),
        _ => None,
    };

    // Signed out there is no token to hold, so the section would be a button
    // that cannot work.
    let Some(username) = username else {
        return rsx! {};
    };

    rsx! {
        Section { title: "API access",
            div { class: "py-3 space-y-2",
                p { class: "text-sm",
                    "Personal API token for "
                    span { class: "font-mono", "{username}" }
                    "."
                }
                p { class: "text-xs opacity-60",
                    "Generating a token replaces any existing one, which stops working immediately."
                }
                button {
                    class: "btn btn-sm",
                    onclick: move |_| async move {
                        error.set(None);
                        match crate::api::client() {
                            Err(e) => error.set(Some(e)),
                            Ok(client) => match client.regenerate_api_token().await {
                                Ok(response) => issued.set(Some(response.token)),
                                Err(e) => error.set(Some(e.to_string())),
                            },
                        }
                    },
                    "Generate new token"
                }

                if let Some(token) = issued() {
                    div { class: "alert alert-warning text-sm flex-col items-start gap-1",
                        span { "Copy this now — it is not shown again." }
                        code { class: "font-mono text-xs break-all", "{token}" }
                    }
                }
                if let Some(e) = error() {
                    div { class: "alert alert-error text-sm", span { "{e}" } }
                }
            }
        }
    }
}

/// What the auto-update schedule's row says about timezones.
#[derive(Clone, PartialEq, Debug)]
struct ScheduleZone {
    description: String,
    warning: Option<String>,
}

/// The schedule's description, naming the server timezone its hours are in,
/// and a warning when that is not the viewer's.
///
/// Compared by current offset rather than by name: `UTC` and `Etc/UTC` are
/// the same clock, and a server that only knows its offset has no name to
/// compare. Two zones that agree now but switch to summer time on different
/// dates are the price, and they disagree by an hour for a few weeks at most.
fn schedule_zone(server: Option<&Timezone>, viewer: &Timezone) -> ScheduleZone {
    const BASE: &str = "When to rebuild what is out of date, as crontab: minute hour \
        day-of-month month day-of-week. H picks a fixed value of this server's own, \
        so H 3 * * * is some minute past 3; @hourly, @daily and @weekly pick them all. \
        @never (or empty) turns it off.";
    let Some(server) = server else {
        return ScheduleZone {
            description: BASE.to_string(),
            warning: None,
        };
    };

    let ahead = server.utc_offset - viewer.utc_offset;
    let warning = (ahead != 0).then(|| {
        let gap = ahead.unsigned_abs() / 60;
        let gap = match (gap / 60, gap % 60) {
            (hours, 0) => format!("{hours}h"),
            (0, minutes) => format!("{minutes}m"),
            (hours, minutes) => format!("{hours}h{minutes:02}m"),
        };
        let direction = if ahead > 0 { "ahead of" } else { "behind" };
        format!(
            "The server's clock is {gap} {direction} yours ({}): the hours above are server time.",
            viewer.describe()
        )
    });

    ScheduleZone {
        description: format!(
            "{BASE} Hours are in the server's timezone, {}.",
            server.describe()
        ),
        warning,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Editor, ScheduleNote, ScheduleZone, SettingRow, is_stored, schedule_note, schedule_zone,
    };
    use aurcache_client::{SchedulePreview, Setting, SettingSource, Timezone};
    use dioxus::prelude::*;

    /// The editor the duration settings use.
    fn duration_editor() -> Editor {
        Editor::Text {
            placeholder: "1h".to_string(),
            wide: false,
        }
    }

    /// An `EventHandler` can only be built inside a running runtime, so the row
    /// is rendered through a host component rather than by handing it props
    /// from outside.
    #[component]
    fn Harness(source: SettingSource, editor: Editor) -> Element {
        rsx! {
            SettingRow {
                setting: Setting::VersionCheckInterval,
                label: "Version check interval",
                description: "how often",
                value: "1h",
                source,
                editor,
                save: move |_| {},
            }
        }
    }

    fn row(source: SettingSource, editor: Editor) -> String {
        let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { source, editor });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn zone(name: Option<&str>, utc_offset: i32) -> Timezone {
        Timezone {
            name: name.map(str::to_string),
            utc_offset,
        }
    }

    /// The schedule names the zone its hours are in, and says nothing more
    /// when that is the viewer's own.
    #[test]
    fn a_schedule_in_the_viewers_timezone_names_it_without_a_warning() {
        let paris = zone(Some("Europe/Paris"), 7200);
        let ScheduleZone {
            description,
            warning,
        } = schedule_zone(Some(&paris), &paris);
        assert!(
            description.ends_with("in the server's timezone, Europe/Paris (UTC+02:00)."),
            "{description}"
        );
        assert_eq!(warning, None);
    }

    /// `UTC` and `Etc/UTC` are the same clock, and a server that knows only
    /// its offset has no name to disagree with.
    #[test]
    fn differently_named_zones_at_the_same_offset_are_not_a_mismatch() {
        let viewer = zone(Some("UTC"), 0);
        assert_eq!(
            schedule_zone(Some(&zone(Some("Etc/UTC"), 0)), &viewer).warning,
            None
        );
        assert_eq!(schedule_zone(Some(&zone(None, 0)), &viewer).warning, None);
    }

    /// The warning says which way and how far, since that is the arithmetic
    /// the viewer would otherwise have to do to know when the schedule fires.
    #[test]
    fn a_schedule_in_another_timezone_warns_by_how_far() {
        let viewer = zone(Some("America/New_York"), -4 * 3600);
        let warning = schedule_zone(Some(&zone(None, 7200)), &viewer).warning;
        assert_eq!(
            warning.as_deref(),
            Some(
                "The server's clock is 6h ahead of yours (America/New_York (UTC-04:00)): \
                 the hours above are server time."
            )
        );

        let india = zone(Some("Asia/Kolkata"), 19800);
        let utc = zone(Some("Etc/UTC"), 0);
        let warning = schedule_zone(Some(&utc), &india).warning.unwrap();
        assert!(warning.contains("5h30m behind yours"), "{warning}");
    }

    /// A server that predates reporting its timezone gets the plain
    /// description rather than a guess.
    #[test]
    fn a_server_without_a_timezone_gets_the_plain_description() {
        let ScheduleZone {
            description,
            warning,
        } = schedule_zone(None, &zone(Some("Europe/Paris"), 7200));
        assert!(!description.contains("timezone"), "{description}");
        assert_eq!(warning, None);
    }

    /// Reset clears a stored row. `Default` has no row to clear, and `Env` is
    /// not stored in the database at all — offering Reset there would delete
    /// nothing while implying the value would change.
    #[test]
    fn only_a_stored_value_can_be_reset() {
        assert!(is_stored(SettingSource::Global));
        assert!(is_stored(SettingSource::Package));
        assert!(!is_stored(SettingSource::Default));
        assert!(!is_stored(SettingSource::Env));
    }

    /// An environment variable outranks anything stored, so an editable field
    /// would take a change that the next read silently discards.
    #[test]
    fn an_env_locked_setting_cannot_be_edited() {
        let html = row(SettingSource::Env, duration_editor());
        assert!(
            html.contains("disabled"),
            "input should be disabled: {html}"
        );
        assert!(
            !html.contains(">Reset<"),
            "reset would clear nothing: {html}"
        );
        // Named from the setting's own metadata, so the banner cannot point at
        // a variable the server does not read.
        assert!(
            html.contains("VERSION_CHECK_INTERVAL"),
            "should name the variable: {html}"
        );
    }

    /// The two unstored sources look identical in the value column — the badge
    /// is the only thing that separates "nobody set this" from "the deployment
    /// set this".
    #[test]
    fn an_unset_setting_says_it_is_the_default() {
        let html = row(SettingSource::Default, duration_editor());
        assert!(html.contains("default"), "{html}");
        assert!(!html.contains(">Reset<"), "nothing stored to reset: {html}");
    }

    /// The variable a deployment would use to pin this setting is documented
    /// on the row whether or not anything has set it — a name that appears only
    /// once it is too late to be useful is not documentation. It lives in hover
    /// text, so it is present in the markup without occupying the row.
    #[test]
    fn a_settings_environment_variable_is_always_named() {
        for source in [
            SettingSource::Default,
            SettingSource::Global,
            SettingSource::Package,
        ] {
            let html = row(source, duration_editor());
            assert!(
                html.contains(
                    r#"title="Set $VERSION_CHECK_INTERVAL to pin this from the environment""#
                ),
                "{source:?}: {html}"
            );
            assert!(
                !html.contains("unset $VERSION_CHECK_INTERVAL"),
                "{source:?} is not pinned, so nothing needs unsetting: {html}"
            );
        }
    }

    /// A stored value is the only case with something to undo.
    #[test]
    fn a_stored_setting_offers_a_reset() {
        let html = row(SettingSource::Global, duration_editor());
        assert!(html.contains(">Reset<"), "{html}");
        assert!(!html.contains("disabled"), "{html}");
    }

    /// Each editor renders its own control, so a row cannot silently fall back
    /// to a text box for a boolean.
    #[test]
    fn each_editor_renders_its_own_control() {
        let text = row(
            SettingSource::Global,
            Editor::Text {
                placeholder: "0 0 3 * * *".to_string(),
                wide: false,
            },
        );
        assert!(text.contains("0 0 3 * * *"), "{text}");

        let toggle = row(SettingSource::Global, Editor::Toggle);
        assert!(toggle.contains("toggle"), "{toggle}");
        assert!(toggle.contains(r#"type="checkbox""#), "{toggle}");
    }

    /// Each answer reads as itself, and only a refused value as an error.
    #[test]
    fn the_schedule_line_says_when_or_why_not() {
        let date = |ts: i64| format!("<{ts}>");
        assert_eq!(
            schedule_note(&SchedulePreview::Runs { at: vec![60, 120] }, date),
            ScheduleNote {
                text: "Next runs, your time: <60>, then <120>".to_string(),
                invalid: false,
            }
        );
        assert!(!schedule_note(&SchedulePreview::Never, date).invalid);
        assert!(!schedule_note(&SchedulePreview::Disabled, date).invalid);
        let refused = schedule_note(
            &SchedulePreview::Invalid {
                reason: "\"* * *\" has 3 fields".to_string(),
            },
            date,
        );
        assert!(refused.invalid);
        assert!(refused.text.contains("3 fields"));
    }
}
