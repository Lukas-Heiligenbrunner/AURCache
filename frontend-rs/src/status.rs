//! Build status as a coloured badge.

use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;

/// The colour a state is shown in, independent of what it is called.
///
/// Shared so a package and a build in the same state read the same, while each
/// keeps its own wording: a package is "up to date", the build that produced it
/// is "successful".
fn badge_class(state: BuildState) -> &'static str {
    match state {
        BuildState::Active => "badge-info",
        BuildState::Successful => "badge-success",
        BuildState::Failed => "badge-error",
        // The two pending states are not failures and not progress, so they
        // stay uncoloured — but visible, or they read as an empty cell.
        // `badge-ghost` has no background on this theme.
        BuildState::Enqueued => "badge-neutral",
        BuildState::WaitingForDeps => "badge-outline",
        // Still under way, like building: the server is doing the last of it.
        BuildState::Publishing => "badge-info",
    }
}

/// A build's status, worded for a build rather than for a package.
#[component]
pub fn BuildStatusBadge(status: BuildState) -> Element {
    let label = status.label();
    let class = badge_class(status);
    rsx! { span { class: "badge {class} badge-sm whitespace-nowrap", "{label}" } }
}

/// A package's status as a coloured badge.
#[component]
pub fn StatusBadge(status: BuildState, out_of_date: bool) -> Element {
    // A package whose last build succeeded but that has newer sources upstream
    // is its own thing: successful, yet needing attention.
    let outdated = status == BuildState::Successful && out_of_date;
    // Worded for a package where that differs: its build succeeding means
    // it is up to date, or would be but for newer sources.
    let label = match status {
        BuildState::Successful if outdated => "out of date",
        BuildState::Successful => "up to date",
        state => state.label(),
    };
    let class = if outdated {
        "badge-warning"
    } else {
        badge_class(status)
    };
    rsx! { span { class: "badge {class} badge-sm whitespace-nowrap", "{label}" } }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render the badge to HTML so its output can be asserted on.
    fn render(status: BuildState, out_of_date: bool) -> String {
        let mut dom = VirtualDom::new_with_props(
            StatusBadge,
            StatusBadgeProps {
                status,
                out_of_date,
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// Each build state must render its own label and colour. This is the test
    /// that would have caught `WaitingForDeps` silently rendering as "unknown"
    /// while the badges were matched on bare integers.
    #[test]
    fn every_build_state_has_its_own_badge() {
        for (state, label, class) in [
            (BuildState::Active, "building", "badge-info"),
            (BuildState::Successful, "up to date", "badge-success"),
            (BuildState::Failed, "failed", "badge-error"),
            (BuildState::Enqueued, "enqueued", "badge-neutral"),
            (
                BuildState::WaitingForDeps,
                "waiting for deps",
                "badge-outline",
            ),
            (BuildState::Publishing, "publishing", "badge-info"),
        ] {
            let status = state;
            let html = render(status, false);
            assert!(
                html.contains(label),
                "{state:?} should render {label:?}: {html}"
            );
            assert!(html.contains(class), "{state:?} should use {class}: {html}");
        }
    }

    /// A successful build that is out of date reads differently from one that
    /// is current, and the distinction is easy to invert.
    #[test]
    fn a_successful_but_outdated_build_is_flagged() {
        let html = render(BuildState::Successful, true);
        assert!(html.contains("out of date"), "{html}");
        assert!(html.contains("badge-warning"), "{html}");
    }

    /// No state may render as `badge-ghost`.
    ///
    /// It has no background on this theme, so "enqueued" and "waiting for
    /// deps" showed as bare text beside the coloured badges and read as an
    /// empty cell rather than a status. The other tests here assert on class
    /// names, which cannot tell whether the result is legible — so this pins
    /// the one class that is known not to be.
    #[test]
    fn no_state_renders_as_an_invisible_badge() {
        for status in [
            BuildState::Active,
            BuildState::Successful,
            BuildState::Failed,
            BuildState::Enqueued,
            BuildState::WaitingForDeps,
            BuildState::Publishing,
        ] {
            for out_of_date in [false, true] {
                let html = render(status, out_of_date);
                assert!(
                    !html.contains("badge-ghost"),
                    "status {status:?} (out_of_date {out_of_date}) renders invisibly: {html}"
                );
            }
        }
    }
}
