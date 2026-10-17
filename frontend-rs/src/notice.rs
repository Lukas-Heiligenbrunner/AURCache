//! A one-off message carried across a redirect.
//!
//! A link to something that no longer exists still lands somewhere useful: a
//! missing package sends you to add it, a missing build to its package's builds,
//! a missing worker to the fleet. The page you land on did not choose to be
//! there, so the notice says why -- and it has to outlive the navigation that
//! caused it, which is why it lives in the shell rather than on either page.

use crate::routes::Route;
use dioxus::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// Nothing went wrong, but you are not where the link said.
    Info,
    /// There was nowhere sensible to go.
    Error,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Notice {
    pub text: String,
    pub level: Level,
    /// The route the notice belongs to. It shows there, and goes once you
    /// have been there and moved on.
    pub on: Route,
    /// Whether `on` has been reached. Until then the old route is still the
    /// current one, and must not count as having moved on.
    arrived: bool,
}

/// Leave for `to`, explaining why when you get there.
///
/// A replace, not a push: the page that redirected has nothing to show, and
/// Back returning to it would only redirect again.
pub fn redirect(mut notice: Signal<Option<Notice>>, to: Route, level: Level, text: String) {
    notice.set(Some(Notice {
        text,
        level,
        on: to.clone(),
        arrived: false,
    }));
    navigator().replace(to);
}

/// Create the slot. The shell does this once, above both the pages that
/// redirect and the toast that shows the result.
pub fn use_notice_provider() {
    use_context_provider(|| Signal::new(None::<Notice>));
}

/// The slot, for a screen that may need to redirect. Taken at the top of a
/// component like any other hook.
pub fn use_notice() -> Signal<Option<Notice>> {
    use_context()
}

/// Where the notice renders: above everything, dialogs included, since the
/// add page a missing package leads to is itself a dialog.
#[component]
pub fn NoticeToast() -> Element {
    let mut notice = use_notice();
    let route = use_route::<Route>();

    // Arrive, then expire on the next move.
    use_effect(use_reactive(&route, move |route| {
        let current = notice.peek().clone();
        let Some(current) = current else { return };
        if current.on == route {
            if !current.arrived {
                notice.set(Some(Notice {
                    arrived: true,
                    ..current
                }));
            }
        } else if current.arrived {
            notice.set(None);
        }
    }));

    let Some(shown) = notice.read().clone() else {
        return rsx! {};
    };
    if shown.on != route {
        return rsx! {};
    }
    let class = match shown.level {
        Level::Info => "alert-info",
        Level::Error => "alert-error",
    };
    rsx! {
        div { class: "toast toast-top toast-center z-[1000]",
            div { class: "alert {class} shadow-lg", role: "status",
                span { "{shown.text}" }
                button {
                    class: "btn btn-ghost btn-xs",
                    aria_label: "Dismiss",
                    onclick: move |_| notice.set(None),
                    "✕"
                }
            }
        }
    }
}
