//! Keeping a screen's data fresh without a manual reload.
//!
//! `use_resource` fetches on mount and then never again on its own. The lists
//! and detail pages are things people leave open while builds run, so each one
//! calls [`use_poll`] to re-fetch on a timer: briskly while something is in
//! flight, slowly when nothing is, and not at all while the tab is hidden — a
//! build server sitting in a background tab should not emit a request a minute
//! forever.
//!
//! That timer is the coarse safety net for everything the browser cannot
//! observe directly: a build finishing, a scheduled version check, another
//! operator's change. The one case it would make wait needlessly is "I just
//! added a package" — so [`PACKAGES_REVISION`] lets the add flow announce that
//! the moment it lands, and [`use_refetch_on_package_change`] re-fetches at
//! once instead of on the next tick.

use dioxus::prelude::*;
use std::time::Duration;

/// Re-fetch cadence while something on the page is still happening.
const ACTIVE: Duration = Duration::from_secs(5);
/// Re-fetch cadence when nothing is — a backstop, not a live feed.
const IDLE: Duration = Duration::from_secs(60);
/// How long to wait between checks while the tab is hidden. Short so returning
/// to it refreshes promptly; no fetch happens on these ticks.
const HIDDEN: Duration = Duration::from_secs(2);

/// Re-run `resource` on an interval: [`ACTIVE`] while `busy`, [`IDLE`]
/// otherwise. Paused while the document is hidden, with a refetch on the first
/// tick after it becomes visible again.
///
/// `busy` is read every render, so a page that goes from idle to active
/// shortens the wait in progress (within one [`CHUNK`]), not just the next
/// one: an idle page would otherwise take a whole idle interval to notice a
/// build that started elsewhere.
pub fn use_poll<T: 'static>(mut resource: Resource<T>, busy: bool) {
    // The loop reads this rather than closing over `busy` directly, because the
    // future is spawned once and `busy` changes over the life of the page.
    let mut interval = use_signal(|| pick(busy));
    use_effect(use_reactive(&busy, move |busy| interval.set(pick(busy))));

    use_future(move || async move {
        let mut was_hidden = false;
        loop {
            if hidden() {
                was_hidden = true;
                gloo_timers::future::sleep(HIDDEN).await;
                continue;
            }
            if was_hidden {
                // Just came back: refetch at once instead of waiting out a
                // full idle interval on the stale data the hidden tab kept.
                was_hidden = false;
            } else {
                sleep_interval(interval).await;
                // Re-check rather than trust the value from before the sleep:
                // the tab may have been hidden the whole time.
                if hidden() {
                    continue;
                }
            }
            resource.restart();
        }
    });
}

/// The wait granularity: an idle→active flip applies at the next chunk
/// boundary rather than after the whole idle interval.
const CHUNK: Duration = Duration::from_secs(5);

/// Sleep `interval()`, re-reading it every [`CHUNK`] so a flip applies
/// mid-wait: shrinking the interval below the time already waited ends the
/// wait, growing it extends it. Returns early when the tab hides.
async fn sleep_interval(interval: Signal<Duration>) {
    let mut waited = Duration::ZERO;
    loop {
        let wait = interval();
        if waited >= wait {
            break;
        }
        let chunk = (wait - waited).min(CHUNK);
        gloo_timers::future::sleep(chunk).await;
        if hidden() {
            break;
        }
        waited += chunk;
    }
}

const fn pick(busy: bool) -> Duration {
    if busy { ACTIVE } else { IDLE }
}

/// Whether the tab is currently not visible. Any failure to tell is treated as
/// visible: a missed pause costs one request, a wrong "hidden" would freeze the
/// page silently.
pub(crate) fn hidden() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .is_some_and(|d| d.hidden())
}

/// Bumped whenever this browser lands a package on the server — an add
/// completed, a restore imported one. [`use_refetch_on_package_change`] watches
/// it so the package list reflects the add immediately instead of waiting out
/// [`use_poll`]'s interval.
///
/// A `GlobalSignal` rather than a provided context because it has no natural
/// owner in the tree: the writer is a detached future in [`crate::progress`],
/// the reader is a screen, and neither is an ancestor of the other. Nothing
/// else needs it — build status, version checks and other operators' changes
/// all ride the poll.
pub static PACKAGES_REVISION: GlobalSignal<u64> = Signal::global(|| 0);

/// Record that a package add or import has landed.
pub fn packages_changed() {
    *PACKAGES_REVISION.write() += 1;
}

/// Re-fetch `resource` whenever [`packages_changed`] is called after this hook
/// mounts. The value present at mount is ignored, so this does not double up
/// with the resource's own initial load.
pub fn use_refetch_on_package_change<T: 'static>(mut resource: Resource<T>) {
    let mut seen = use_signal(|| *PACKAGES_REVISION.peek());
    use_effect(move || {
        let now = PACKAGES_REVISION();
        if now != *seen.peek() {
            seen.set(now);
            resource.restart();
        }
    });
}
