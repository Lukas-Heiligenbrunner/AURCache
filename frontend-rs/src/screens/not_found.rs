//! Catch-all for a fragment that matches no route.

use crate::routes::Route;
use dioxus::prelude::*;

#[component]
pub fn NotFound(segments: Vec<String>) -> Element {
    let path = segments.join("/");

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body items-center text-center py-16",
                h2 { class: "card-title", "Not found" }
                p { class: "opacity-70 font-mono text-sm", "/{path}" }
                Link { class: "btn btn-primary btn-sm mt-2", to: Route::Dashboard {}, "Go to dashboard" }
            }
        }
    }
}
