//! A Rust frontend for AURCache, in Dioxus.
//!
//! Shares the API types with the server through `aurcache-common` rather than
//! redeclaring them, so a change to a response shape becomes a compile error
//! here instead of a silent drift between two hand-maintained models.
//!
//! The port is partial. Routes that are not implemented render a "not ported"
//! card, so the gap is visible rather than hidden.

// `rsx!` turns `"{value}"` interpolation into a `format!`, and the lint reads
// that expansion as redundant. Its suggested `.to_string()` is not valid where
// a text node is expected, so every hit is a false positive from macro output.
#![allow(clippy::useless_format)]

mod api;
mod build_log;
mod clipboard;
mod dates;
mod format;
mod listing;
mod log_tail;
mod notice;
mod platforms;
mod poll;
mod progress;
mod routes;
mod screens;
mod setting_editor;
mod shell;
mod source_editor;
mod status;
mod theme;

use dioxus::prelude::*;
use routes::Route;

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    // Routes are real paths. That works on a reload or a pasted link because
    // the server answers unknown non-API paths with the app shell; see
    // `aurcache_api::spa`.
    // Provided here so changing the format in the sidebar re-renders the dates
    // on the current page rather than only on the next navigation.
    dates::use_date_style_provider();
    // Above the router, so an add's progress card survives navigating away
    // from the page that started it -- which is the point of it existing.
    progress::use_jobs_provider();

    rsx! {
        Router::<Route> {}
        progress::ProgressOverlay {}
    }
}
