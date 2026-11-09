//! Small pieces of markup several screens share: a dialog, a confirmation, a
//! spinner, and a table row that is a link.

use crate::routes::Route;
use dioxus::prelude::*;

/// A dialog over the page.
///
/// `open` rather than mounting and unmounting: a dialog that is always in the
/// tree keeps its state across openings where a caller wants that, and one
/// mounted only while open passes `true`.
///
/// Clicking away and Escape both close it, as they do any dialog, except while
/// `locked`: a dialog whose request is in flight cannot be dismissed out from
/// under it.
#[component]
pub fn Modal(
    open: bool,
    /// What the dialog is, for assistive tech.
    label: String,
    on_close: EventHandler<()>,
    #[props(default)] locked: bool,
    /// Extra classes for the box, a width usually.
    #[props(default)]
    class: String,
    children: Element,
) -> Element {
    rsx! {
        div {
            class: if open { "modal modal-open" } else { "modal" },
            role: "dialog",
            aria_modal: "true",
            aria_label: "{label}",
            div {
                class: "modal-box {class}",
                // On the box rather than a field, so it works wherever focus
                // has moved to inside: keydown bubbles from it.
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape && !locked {
                        on_close.call(());
                    }
                },
                {children}
            }
            button {
                class: "modal-backdrop",
                disabled: locked,
                onclick: move |_| on_close.call(()),
                "Close"
            }
        }
    }
}

/// What confirming a [`ConfirmDialog`] does, and so how its button is coloured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Consequence {
    /// It cannot be undone.
    Destructive,
    /// It throws something away that could be kept.
    Discards,
}

impl Consequence {
    const fn button(self) -> &'static str {
        match self {
            Self::Destructive => "btn btn-error btn-sm",
            Self::Discards => "btn btn-warning btn-sm",
        }
    }
}

/// A yes/no question before something that cannot be taken back.
///
/// Backing out is the default, and what clicking away means. `children` is the
/// explanation under the title.
#[component]
pub fn ConfirmDialog(
    open: bool,
    label: String,
    title: String,
    /// The button that backs out.
    cancel: String,
    /// The button that goes ahead.
    confirm: String,
    consequence: Consequence,
    /// The confirmed request is in flight: both buttons wait for it.
    #[props(default)]
    busy: bool,
    on_cancel: EventHandler<()>,
    on_confirm: EventHandler<()>,
    children: Element,
) -> Element {
    rsx! {
        Modal { open, label, on_close: on_cancel, locked: busy,
            h3 { class: "font-bold text-lg", "{title}" }
            p { class: "text-sm opacity-70 pt-2", {children} }
            div { class: "modal-action",
                button {
                    class: "btn btn-sm",
                    disabled: busy,
                    onclick: move |_| on_cancel.call(()),
                    "{cancel}"
                }
                button {
                    class: consequence.button(),
                    disabled: busy,
                    onclick: move |_| on_confirm.call(()),
                    if busy {
                        span { class: "loading loading-spinner loading-xs" }
                    }
                    "{confirm}"
                }
            }
        }
    }
}

/// Something is loading and there is nothing else to show yet.
#[component]
pub fn Spinner() -> Element {
    rsx! {
        div { class: "flex justify-center p-8",
            span { class: "loading loading-spinner loading-lg" }
        }
    }
}

/// A table row that is one link: clicking anywhere in it goes to `to`.
///
/// Pair it with a [`RowLink`] on the cell that names the row. A bare row
/// handler gives no address to copy, no middle-click and nothing to focus.
#[component]
pub fn LinkRow(to: Route, #[props(default)] class: String, children: Element) -> Element {
    rsx! {
        tr {
            class: "hover cursor-pointer {class}",
            onclick: move |_| {
                navigator().push(to.clone());
            },
            {children}
        }
    }
}

/// The real link inside a [`LinkRow`], so the row has an address to copy, a
/// middle-click target and something to focus.
///
/// Not `link link-primary`: when every cell in the row navigates, underlining
/// one implies the rest do not.
#[component]
pub fn RowLink(to: Route, #[props(default)] class: String, children: Element) -> Element {
    rsx! {
        Link {
            class,
            to,
            // Otherwise the click reaches the row too and pushes the same route
            // twice, leaving a duplicate history entry.
            onclick: move |e: MouseEvent| e.stop_propagation(),
            {children}
        }
    }
}
