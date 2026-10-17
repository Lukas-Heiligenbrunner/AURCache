//! The app frame: side menu plus the content area every screen renders into.
//!
//! Laid out as a daisyUI drawer, which is permanent from `lg` up and slides
//! over the content below it, without needing to measure the viewport in Rust.

use crate::dates::DateStylePicker;
use crate::listing::ViewParams;
use crate::routes::{MenuEntry, Route};
use crate::theme::ThemePicker;
use dioxus::prelude::*;

/// Where the docs and the source live. Rendered as real links rather than
/// click handlers so they behave like links — middle-click, copy address.
const DOCS_URL: &str =
    "https://lukas-heiligenbrunner.github.io/AURCache/docs/overview/introduction";
const GITHUB_URL: &str = "https://github.com/Lukas-Heiligenbrunner/AURCache";

/// Checkbox id the drawer toggle and its labels agree on.
const DRAWER_ID: &str = "menu-drawer";

#[component]
pub fn MenuShell() -> Element {
    crate::notice::use_notice_provider();
    rsx! {
        crate::notice::NoticeToast {}
        div { class: "drawer lg:drawer-open",
            input { id: DRAWER_ID, r#type: "checkbox", class: "drawer-toggle" }

            // `h-screen` rather than `min-h-screen`, with the scrolling moved
            // inside: a page that wants to fill the window and scroll one pane
            // of itself -- a build log -- needs an ancestor with a height to
            // fill. Against `min-h-screen` there is none, so such a page can
            // only guess at a height, overshoot, and leave the window scrolling
            // behind the pane that is already scrolling.
            div { class: "drawer-content flex flex-col h-screen overflow-hidden bg-base-200",
                // Only exists below `lg`, where the drawer is collapsed.
                div { class: "navbar bg-base-100 shadow-sm lg:hidden",
                    label {
                        r#for: DRAWER_ID,
                        class: "btn btn-square btn-ghost drawer-button",
                        MenuIcon {}
                    }
                    span { class: "font-bold px-2", "AURCache" }
                }

                // The one scrollbar for pages that simply run long. A page
                // that fills this exactly -- see the build log -- leaves it
                // with nothing to scroll, and its own pane is the only one.
                main { class: "flex-1 min-h-0 overflow-y-auto p-4 lg:p-6", Outlet::<Route> {} }
            }

            div { class: "drawer-side z-10",
                label {
                    r#for: DRAWER_ID,
                    class: "drawer-overlay",
                    aria_label: "Close menu",
                }
                SideMenu {}
            }
        }
    }
}

#[component]
fn SideMenu() -> Element {
    // The live route, so the active entry is derived rather than tracked in a
    // signal that could fall out of step with the URL. The mapping itself lives
    // on `Route` so it can be tested without rendering.
    let active = use_route::<Route>().menu_entry();
    let mut preferences_open = use_signal(|| false);
    // The running server's version, not this crate's: the two only agree when
    // the UI asks. `None` until the first fetch lands (and if it never does),
    // when the display falls back to the baked-in crate version below.
    let mut server_version = use_signal(|| None::<String>);
    use_future(move || async move {
        if let Ok(client) = crate::api::client()
            && let Ok(info) = client.server_info().await
        {
            server_version.set(Some(info.version));
        }
    });

    rsx! {
            aside { class: "bg-base-100 w-64 min-h-full flex flex-col",
                div { class: "flex items-center gap-3 px-5 pt-6 pb-4",
                    LogoIcon {}
                    div {
                        div { class: "font-black text-base leading-tight", "AURCache" }
                        div { class: "text-xs opacity-70 leading-tight",
                            "The Archlinux AUR"
                            br {}
                            "build server"
                        }
                    }
                }

                // The one thing you *create* here, reachable without first
                // navigating to Packages. A button rather than a menu row on
                // purpose: every row below is a section of the app you dwell
                // in, and styling a single narrow task as a sixth section
                // would misdescribe both it and them.
                div { class: "px-5 pb-2",
                    Link {
                        to: Route::PackageAdd { q: String::new() },
                        class: "btn btn-primary btn-sm w-full gap-2",
                        PlusIcon {}
                        "Add package"
                    }
                }

                MenuSection { title: "General",
                    MenuLink {
                        to: Route::Dashboard {},
                        label: "Dashboard",
                        active: active == Some(MenuEntry::Dashboard),
                        icon: rsx! { DashboardIcon {} },
                    }
                    MenuLink {
                        to: Route::Packages { view: ViewParams::default(), q: String::new() },
                        label: "Packages",
    active: active == Some(MenuEntry::Packages),
                        icon: rsx! { PackagesIcon {} },
                    }
                    MenuLink {
                        to: Route::Builds { view: ViewParams::default(), q: String::new() },
                        label: "Builds",
    active: active == Some(MenuEntry::Builds),
                        icon: rsx! { BuildsIcon {} },
                    }
                    MenuLink {
                        to: Route::Logs { view: crate::listing::ViewParams::default() },
                        label: "Logs",
                        active: active == Some(MenuEntry::Logs),
                        icon: rsx! { LogsIcon {} },
                    }
                    MenuLink {
                        to: Route::Workers {},
                        label: "Workers",
                        active: active == Some(MenuEntry::Workers),
                        icon: rsx! { WorkersIcon {} },
                    }
                }

                MenuSection { title: "Settings",
                    MenuLink {
                        to: Route::Settings {},
                        label: "Settings",
                        active: active == Some(MenuEntry::Settings),
                        icon: rsx! { SettingsIcon {} },
                    }
                    ExternalMenuLink { href: DOCS_URL, label: "Help" }
                    // Client-side only: a display preference, kept in the
                    // browser rather than in server settings. See `crate::theme`.
                    MenuButton {
                    label: "Preferences",
                    onclick: move |_| preferences_open.set(true),
                    icon: rsx! { SlidersIcon {} },
                }
                }

                div { class: "flex-1" }

                PreferencesDialog { open: preferences_open }

            MenuSection { title: "Project info",
                    ExternalMenuLink { href: GITHUB_URL, label: "GitHub" }
                    div { class: "px-5 pt-1 pb-4 text-xs opacity-50",
                        "Version {display_version(server_version.read().as_deref())}"
                    }
                }
            }
        }
}

/// What the sidebar's version line shows: the running server's version once
/// the fetch lands, otherwise the crate version baked in at compile time.
/// The fallback only matters before the first fetch, or when the server
/// cannot be reached at all — the bundled UI always agrees with its server.
fn display_version(server_version: Option<&str>) -> String {
    server_version
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .to_string()
}

#[component]
fn MenuSection(title: String, children: Element) -> Element {
    rsx! {
        div {
            div { class: "flex items-center gap-3 px-5 pt-2 pb-2",
                span { class: "text-xs opacity-60 whitespace-nowrap", "{title}" }
                div { class: "h-px bg-base-content/20 flex-1" }
            }
            nav { class: "flex flex-col", {children} }
        }
    }
}

/// One navigation entry.
///
/// A `Link` rather than an onclick handler, so it renders a real `href`: the
/// address is copyable and the browser handles modifier-clicks itself.
#[component]
fn MenuLink(to: Route, label: String, active: bool, icon: Element) -> Element {
    let classes = if active {
        "flex items-center gap-4 px-5 py-2.5 bg-primary text-primary-content font-bold"
    } else {
        "flex items-center gap-4 px-5 py-2.5 hover:bg-base-200"
    };

    rsx! {
        Link {
            to,
            class: classes,
            // No `aria_current` here on purpose: `Link` already sets it when
            // the href is exactly the current route. `active` is broader — it
            // stays on for a whole section, so `/build/3` keeps "Builds" lit —
            // and marking that as `aria-current="page"` would tell a screen
            // reader the section link is the page being viewed, which it is not.
            {icon}
            span { class: "text-sm", "{label}" }
        }
    }
}

/// A menu entry that acts rather than navigates.
///
/// A `button`, not a link: it opens something in place, and rendering it as an
/// anchor would offer a middle-click that goes nowhere.
#[component]
fn MenuButton(label: String, onclick: EventHandler<MouseEvent>, icon: Element) -> Element {
    rsx! {
        button {
            class: "flex items-center gap-4 px-5 py-2.5 hover:bg-base-200 w-full text-left",
            onclick: move |e| onclick.call(e),
            {icon}
            span { class: "text-sm", "{label}" }
        }
    }
}

/// Display preferences, kept out of the sidebar itself.
///
/// They are per-browser rather than account settings — the server supplies a
/// default and this overrides it locally — so the dialog says so rather than
/// leaving someone to wonder why a colleague sees something else.
#[component]
fn PreferencesDialog(open: Signal<bool>) -> Element {
    let mut open = open;

    rsx! {
        div {
            class: if open() { "modal modal-open" } else { "modal" },
            role: "dialog",
            aria_modal: "true",
            aria_label: "UI preferences",
            div { class: "modal-box",
                h3 { class: "font-bold text-lg", "UI preferences" }
                p { class: "text-sm opacity-60",
                    "Stored in this browser, not on the server."
                }
                div { class: "pt-2",
                    ThemePicker {}
                    DateStylePicker {}
                }
                div { class: "modal-action",
                    button {
                        class: "btn btn-sm",
                        onclick: move |_| open.set(false),
                        "Done"
                    }
                }
            }
            // Clicking away closes it, which is what the backdrop is for.
            button {
                class: "modal-backdrop",
                onclick: move |_| open.set(false),
                aria_label: "Close preferences",
                "Close"
            }
        }
    }
}

/// Leaves the app, so it is a plain anchor rather than a router `Link`.
#[component]
fn ExternalMenuLink(href: String, label: String) -> Element {
    rsx! {
        a {
            class: "flex items-center gap-4 px-5 py-2.5 hover:bg-base-200",
            href,
            target: "_blank",
            rel: "noopener noreferrer",
            ExternalIcon {}
            span { class: "text-sm", "{label}" }
        }
    }
}

// ---------------------------------------------------------------------------
// Icons
//
// Inline SVG paths rather than asset files: the prototype has no asset
// pipeline, and inlining keeps the menu to one dependency-free module. Each is
// 20x20 on a 24-unit grid, stroked in `currentColor` so it follows the theme
// and the active-row colour without a second rule.
// ---------------------------------------------------------------------------

#[component]
fn Icon(path: &'static str) -> Element {
    rsx! {
        svg {
            class: "h-5 w-5 shrink-0",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.8",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            path { d: path }
        }
    }
}

/// The project logo, which the server embeds beside the bundle (see
/// `aurcache-api`'s build script).
#[component]
fn LogoIcon() -> Element {
    rsx! {
        img { class: "h-11 w-auto shrink-0", src: "/icon.svg", alt: "" }
    }
}

#[component]
fn MenuIcon() -> Element {
    rsx! { Icon { path: "M4 6h16M4 12h16M4 18h16" } }
}

#[component]
fn DashboardIcon() -> Element {
    rsx! { Icon { path: "M4 13h6V4H4v9Zm10 7h6v-9h-6v9ZM4 20h6v-4H4v4Zm10-11h6V4h-6v5Z" } }
}

#[component]
fn BuildsIcon() -> Element {
    rsx! { Icon { path: "M14 7h7M14 12h7M14 17h7M3 7l2 2 3-3M3 16l2 2 3-3" } }
}

#[component]
fn PackagesIcon() -> Element {
    rsx! {
        Icon { path: "M21 8v8l-9 4-9-4V8l9-4 9 4Zm-18 0 9 4 9-4M12 12v8" }
    }
}

/// Plus, on the sidebar's add-package button.
#[component]
fn PlusIcon() -> Element {
    rsx! { ButtonIcon { path: "M12 5v14M5 12h14" } }
}

/// Button-sized icon base: the 4-unit glyphs that sit in buttons next to
/// text, smaller than the 5-unit `Icon` ones that line a menu row.
#[component]
fn ButtonIcon(path: &'static str) -> Element {
    rsx! {
        svg {
            class: "h-4 w-4 shrink-0",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            path { d: path }
        }
    }
}

/// A funnel, on the log's per-row "narrow to this" menu.
#[component]
pub(crate) fn FilterIcon() -> Element {
    rsx! {
        ButtonIcon { path: "M3 4h18l-7 8.5V19l-4 2v-8.5L3 4Z" }
    }
}

/// Copy-to-clipboard, on the build log's copy button.
#[component]
pub(crate) fn CopyIcon() -> Element {
    rsx! {
        ButtonIcon { path: "M9 5h10a2 2 0 0 1 2 2v10M15 7H5a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V9a2 2 0 0 0-2-2Z" }
    }
}

/// Checkmark, swapped in for `CopyIcon` once the copy has landed.
#[component]
pub(crate) fn CheckIcon() -> Element {
    rsx! { ButtonIcon { path: "M20 6 9 17l-5-5" } }
}

/// Warning, shown on the copy button once the window is a trimmed tail — what
/// it copies is not the whole log any more.
#[component]
pub(crate) fn WarnIcon() -> Element {
    rsx! {
        ButtonIcon { path: "m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Zm-9.73-6v-4m0 4h.01" }
    }
}

/// Download, on the link that fetches the whole log from the server and streams
/// it to the browser's download manager.
#[component]
pub(crate) fn DownloadIcon() -> Element {
    rsx! {
        ButtonIcon { path: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5M12 15V3" }
    }
}

#[component]
fn LogsIcon() -> Element {
    rsx! { Icon { path: "M4 6h16M4 12h16M4 18h10" } }
}

#[component]
fn WorkersIcon() -> Element {
    rsx! {
        Icon { path: "M4 5h16v5H4V5Zm0 9h16v5H4v-5Zm3-6.5h.01M7 16.5h.01" }
    }
}

#[component]
fn SettingsIcon() -> Element {
    rsx! {
        Icon { path: "M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm8-3a8 8 0 0 0-.13-1.4l2-1.6-2-3.4-2.4 1a8 8 0 0 0-2.4-1.4L14.7 2h-4l-.4 2.6a8 8 0 0 0-2.4 1.4l-2.4-1-2 3.4 2 1.6a8 8 0 0 0 0 2.8l-2 1.6 2 3.4 2.4-1a8 8 0 0 0 2.4 1.4l.4 2.6h4l.4-2.6a8 8 0 0 0 2.4-1.4l2.4 1 2-3.4-2-1.6c.08-.46.13-.93.13-1.4Z" }
    }
}

#[component]
fn SlidersIcon() -> Element {
    rsx! {
        Icon { path: "M4 6h10M18 6h2M4 12h4M12 12h8M4 18h10M18 18h2M14 4v4M8 10v4M14 16v4" }
    }
}

#[component]
fn ExternalIcon() -> Element {
    rsx! {
        Icon { path: "M14 4h6v6M20 4l-8 8M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus::history::{History, MemoryHistory};
    use dioxus::router::components::HistoryProvider;
    use std::rc::Rc;

    /// The whole app rendered at one route with an in-memory history, without
    /// a browser.
    ///
    /// Goes through the real `Router`, so this covers what the unit tests on
    /// `Route` cannot: that the route table, the shell, the outlet and the menu
    /// actually compose.
    ///
    /// The history callback is built inside the component on purpose —
    /// `Callback::new` needs the Dioxus runtime, which only exists in scope.
    #[component]
    fn MemoryApp(path: String) -> Element {
        rsx! {
            HistoryProvider {
                history: move |_| Rc::new(MemoryHistory::with_initial_path(path.clone())) as Rc<dyn History>,
                Router::<Route> {}
            }
        }
    }

    fn render_at(path: &str) -> String {
        let mut dom = VirtualDom::new_with_props(
            MemoryApp,
            MemoryAppProps {
                path: path.to_string(),
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The menu is part of the layout, so it renders once around whatever
    /// screen is current — not per screen.
    #[test]
    fn every_screen_renders_inside_the_menu_shell() {
        for path in [
            "/",
            "/settings",
            "/settings/config-files",
            "/workers",
            "/logs",
        ] {
            let html = render_at(path);
            assert!(html.contains("AURCache"), "{path} lost the menu header");
            assert!(html.contains("Dashboard"), "{path} lost the menu: {html}");
            assert!(
                html.contains("drawer-side"),
                "{path} did not render the drawer"
            );
        }
    }

    /// Links are rendered as real `href`s, which is what makes them copyable
    /// and middle-clickable. A handler-only menu would render bare `<a>`s.
    ///
    /// The list routes spread their filter into the query, and dioxus writes
    /// the `?` even when nothing follows it, so those two are `/builds?` until
    /// DioxusLabs/dioxus#5793 lands. Matching the prefix accepts that and
    /// nothing more -- a link that grew real query content would still fail
    /// `an_empty_search_leaves_no_trace_in_the_url`.
    #[test]
    fn menu_entries_are_real_links() {
        let html = render_at("/");
        for href in ["/builds?", "/packages?", "/settings", "/workers"] {
            assert!(
                html.contains(&format!("href=\"{href}\"")),
                "no link to {href}: {html}"
            );
        }
    }

    /// The exact current page is marked for assistive tech, not only coloured.
    #[test]
    fn the_current_page_is_marked_for_assistive_tech() {
        let html = render_at("/settings");
        assert!(html.contains("aria-current=\"page\""), "{html}");
    }

    /// Links are plain paths, which is what the server's app-shell fallback
    /// makes work. A stray `#` here would mean the router was still routing on
    /// the fragment.
    #[test]
    fn links_are_paths_not_fragments() {
        let html = render_at("/");
        for route in ["/builds?", "/packages?", "/settings"] {
            assert!(
                html.contains(&format!("href=\"{route}\"")),
                "no plain-path link to {route}: {html}"
            );
        }
        assert!(
            !html.contains("href=\"#"),
            "a fragment href survived: {html}"
        );
    }

    /// The version line prefers the running server over the baked-in crate
    /// version, which is what keeps the bundled UI and its server in
    /// agreement. The fallback only shows before the fetch lands.
    #[test]
    fn the_version_line_prefers_the_server_version() {
        assert_eq!(
            display_version(Some("0.5.0+g8afa04a.dirty")),
            "0.5.0+g8afa04a.dirty"
        );
        assert_eq!(display_version(None), env!("CARGO_PKG_VERSION"));
    }

    /// The version line renders even before the server version arrives (SSR
    /// and first paint run no fetches), so the menu never has a hole where
    /// it goes.
    #[test]
    fn the_version_line_renders_without_a_server() {
        let html = render_at("/");
        assert!(
            html.contains(&format!("Version {}", env!("CARGO_PKG_VERSION"))),
            "{html}"
        );
    }

    /// An unknown route still renders the frame, so the menu is there to
    /// navigate away with rather than a bare error page.
    #[test]
    fn an_unknown_route_renders_the_not_found_screen_in_the_shell() {
        let html = render_at("/no/such/page");
        assert!(html.contains("Not found"), "{html}");
        assert!(html.contains("drawer-side"), "menu missing: {html}");
        // Nothing is the current page, so nothing claims to be.
        assert!(!html.contains("aria-current"), "{html}");
    }
}
