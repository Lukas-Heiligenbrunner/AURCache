//! Tests that click and type, against a real browser and a real server.
//!
//! The rest of the suite renders components in isolation and asserts on the
//! markup. That cannot see what a control *does* — a search box bound one way
//! instead of two renders identically on first paint — and it cannot see the
//! URL, which is not markup at all.
//!
//! These run against the stack `scripts/test-frontend.sh` brings up: a server
//! with the fixture data and the built frontend served beside it. They are
//! `#[ignore]`d so a bare `cargo test` stays fast and green without one; the
//! script passes `--ignored` once its ports are answering.
//!
//! WebDriver rather than the devtools protocol, after measuring both. Two
//! things decided it. `thirtyfour` manages the browser process itself —
//! resolving the local Chrome, fetching a matching chromedriver, and shutting
//! both down — where the CDP client needed the script to start a browser on a
//! fixed port, preflight it, and trap its cleanup. And `chromiumoxide`'s
//! `Page::goto` cannot do a fragment-only navigation: it waits for a
//! navigation lifecycle event that a same-document hash change never emits, so
//! `/packages#neofetch` timed out 30 times out of 30. Since this frontend puts
//! its search term in the fragment, that is not a corner we can avoid.
//!
//! Both were equally reliable once used correctly (30/30 each); this one is
//! reliable with less around it.

// Host only. The crate ships as wasm, and `--all-targets` would otherwise try
// to build this — and tokio's mio — for a target it cannot serve.
#![cfg(not(target_arch = "wasm32"))]

use std::time::Duration;
use thirtyfour::prelude::*;

/// How long to wait for the page to reach an expected state.
const PATIENCE: Duration = Duration::from_secs(10);

/// Where the frontend is served. The script exports this; the default matches
/// what it uses, so a stack brought up by hand works too.
fn ui_base() -> String {
    std::env::var("AURCACHE_UI").unwrap_or_else(|_| "http://localhost:8080".to_string())
}

/// A value as a JavaScript literal, so page scripts can be built by hand
/// without worrying about quoting.
fn json(value: &str) -> String {
    serde_json::to_string(value).expect("a string is always serialisable")
}

/// A browser session for the run.
struct Session {
    driver: WebDriver,
}

impl Session {
    /// Start Chrome. thirtyfour downloads and supervises the matching
    /// chromedriver itself, so nothing outside has to.
    async fn start() -> Self {
        let mut caps = DesiredCapabilities::chrome();
        caps.set_headless().expect("headless");
        caps.add_arg("--no-sandbox").expect("no-sandbox");
        caps.add_arg("--disable-gpu").expect("disable-gpu");
        let driver = WebDriver::managed(caps).await.expect("start chrome");
        Self { driver }
    }

    /// Go to a route and wait for the app to mount.
    ///
    /// Local storage is cleared afterwards: this frontend keeps its theme and
    /// date format there, and one scenario's preferences must not colour the
    /// next.
    async fn open(&self, path: &str) {
        self.driver
            .goto(format!("{}{path}", ui_base()))
            .await
            .expect("goto");
        self.wait_for(".drawer-side").await;
        let _: bool = self
            .eval("return (localStorage.clear(), true);".to_string())
            .await;
    }

    /// Evaluate JS in the page, returning `T`.
    ///
    /// Everything below goes through here rather than through element handles.
    /// A handle names a node, and this app re-renders as you type — a git URL
    /// makes the ref and subfolder fields appear — so a handle taken a moment
    /// ago can point at a node no longer in the document.
    async fn eval<T: serde::de::DeserializeOwned + Default>(&self, script: String) -> T {
        match self.driver.execute(script, Vec::new()).await {
            Ok(ret) => ret.convert().unwrap_or_default(),
            Err(_) => T::default(),
        }
    }

    /// Poll a page script until it reports true, or fail showing the page.
    async fn wait_for_script(&self, what: &str, script: String) {
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            if self.eval::<bool>(script.clone()).await {
                return;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "timed out waiting for {what}. Page was:\n{}",
                    self.text().await
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_for(&self, selector: &str) {
        self.wait_for_script(
            &format!("{selector:?} to appear"),
            format!(
                "return document.querySelector({}) !== null;",
                json(selector)
            ),
        )
        .await;
    }

    /// Wait until `check` holds for the page's text, or fail showing it.
    async fn wait_until(&self, what: &str, check: impl Fn(&str) -> bool) {
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            let text = self.text().await;
            if check(&text) {
                return;
            }
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for {what}. Page was:\n{text}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn text(&self) -> String {
        self.eval("return document.body.innerText;".to_string())
            .await
    }

    /// Put `text` in a field, the way a person would as far as the app can tell.
    ///
    /// Assigning the value and dispatching `input` is what the handler reads —
    /// it takes the value off the event's target — and unlike synthesised key
    /// presses it cannot be interrupted half way by a re-render.
    async fn type_into(&self, selector: &str, text: &str) {
        self.wait_for(selector).await;
        let script = format!(
            "const el = document.querySelector({}); if (!el) return false; \
              el.focus(); el.value = {}; \
              el.dispatchEvent(new Event('input', {{ bubbles: true }})); return true;",
            json(selector),
            json(text),
        );
        assert!(
            self.eval::<bool>(script).await,
            "could not type into {selector:?}"
        );
    }

    async fn click(&self, selector: &str) {
        self.wait_for_script(
            &format!("{selector:?} to click"),
            format!(
                "const el = document.querySelector({}); if (!el) return false; \
                  el.click(); return true;",
                json(selector)
            ),
        )
        .await;
    }

    /// Click the element matching `selector` whose text is `text`.
    ///
    /// CSS cannot select on text, and the alternative — labelling buttons for
    /// the tests' benefit — would put scaffolding in the markup people use.
    async fn click_labelled(&self, selector: &str, text: &str) {
        self.wait_for_script(
            &format!("a {selector} labelled {text:?}"),
            format!(
                "const el = [...document.querySelectorAll({})] \
                  .find(e => e.textContent.trim() === {}); \
                  if (!el) return false; el.click(); return true;",
                json(selector),
                json(text),
            ),
        )
        .await;
    }

    /// Pick an option in a `<select>` by its value, firing the change event the
    /// way a person choosing one would.
    ///
    /// Setting `value` alone changes what is displayed and nothing else: the
    /// framework listens for `change`, so without dispatching it the page would
    /// look filtered while showing the same rows.
    async fn select_option(&self, selector: &str, value: &str) {
        self.wait_for_script(
            &format!("{selector:?} to offer {value:?}"),
            format!(
                "const el = document.querySelector({}); \
                 if (!el) return false; el.value = {}; \
                 el.dispatchEvent(new Event('change', {{ bubbles: true }})); return true;",
                json(selector),
                json(value),
            ),
        )
        .await;
    }

    /// How many nodes match, for checking how much of a list is on screen.
    async fn count(&self, selector: &str) -> i64 {
        self.eval(format!(
            "return document.querySelectorAll({}).length;",
            json(selector)
        ))
        .await
    }

    /// The value of an input, for checking that typing landed where intended.
    async fn value_of(&self, selector: &str) -> String {
        self.eval(format!(
            "return document.querySelector({})?.value ?? \"\";",
            json(selector)
        ))
        .await
    }

    async fn url(&self) -> String {
        self.driver
            .current_url()
            .await
            .map(|u| u.to_string())
            .unwrap_or_default()
    }

    /// End the session, which stops both Chrome and the driver.
    async fn stop(self) {
        let _ = self.driver.quit().await;
    }
}

/// The scenarios, in one run over one session.
///
/// One `#[tokio::test]` rather than several: starting a browser per test is
/// both slower and, in the CDP client, where the flakiness lived. The trade is
/// coarser reporting — a failure names the scenario in its message rather than
/// in the test name.
#[tokio::test]
#[ignore = "needs the stack from scripts/test-frontend.sh"]
async fn interactions() {
    let session = Session::start().await;

    source_editor_ignores_superseded_requests(&session).await;
    filtering_narrows_the_list_and_updates_the_url(&session).await;
    a_stored_config_file_is_loaded_into_the_editor(&session).await;
    settings_preserve_edits_in_flight_and_reject_double_submission(&session).await;
    a_linked_search_arrives_applied(&session).await;
    a_linked_status_filter_arrives_applied(&session).await;
    one_queued_package_can_be_taken_back(&session).await;
    a_queue_is_added_as_one_job(&session).await;
    the_export_dialog_warns_only_when_secrets_are_asked_for(&session).await;
    the_restore_dialog_offers_its_options(&session).await;
    approving_a_worker_lets_it_build(&session).await;
    a_workers_settings_say_where_each_value_came_from(&session).await;
    the_log_filters_narrow_what_it_shows(&session).await;
    a_per_package_file_leaves_the_server_wide_one_alone(&session).await;
    a_build_flag_survives_a_reload_and_can_be_taken_off(&session).await;
    dependencies_stay_out_of_the_list_until_asked_for(&session).await;
    following_a_dependency_loads_that_package(&session).await;
    a_second_page_holds_different_builds(&session).await;
    a_build_can_be_found_by_the_name_the_list_shows(&session).await;
    stopping_a_build_asks_first(&session).await;
    the_add_dialog_focuses_its_search(&session).await;
    // Fetches the PKGBUILD from the AUR, like the source editor's route checks.
    if std::env::var("AURCACHE_ONLINE").as_deref() == Ok("1") {
        a_source_edit_can_be_reset_either_way_and_its_patch_read(&session).await;
    }
    every_row_has_a_cell_for_every_column(&session).await;
    a_link_to_something_gone_lands_somewhere_useful(&session).await;
    // Last: it deletes a row the others would otherwise still be looking at.
    removing_a_package_takes_it_out_of_the_list(&session).await;

    session.stop().await;
}

/// A link outlives what it names -- a log entry, a bookmark, a pasted URL --
/// so one to something gone still lands somewhere useful, and says why.
///
/// A missing package is offered for adding, with its name already searched.
/// A missing build of a package that is here falls back to that package's
/// builds; of a package that is not, to adding it. A missing worker falls back
/// to the fleet. Each is a redirect a route check cannot see: the page asked
/// for never renders, and the notice is what explains where you are.
async fn a_link_to_something_gone_lands_somewhere_useful(session: &Session) {
    session.open("/package/no-such-package").await;
    session
        .wait_until("the add page to explain itself", |t| {
            t.contains("No package called no-such-package is tracked here")
        })
        .await;
    assert!(
        session
            .url()
            .await
            .ends_with("/packages/add#no-such-package"),
        "a missing package should offer adding it: {}",
        session.url().await
    );

    session.open("/package/hello/build/999").await;
    session
        .wait_until("the builds page to explain itself", |t| {
            t.contains("hello has no build #999")
        })
        .await;
    assert!(
        session.url().await.ends_with("/package/hello/builds"),
        "a missing build should fall back to its package's builds: {}",
        session.url().await
    );

    session.open("/package/no-such-package/build/3").await;
    session
        .wait_until("the add page to explain itself", |t| {
            t.contains("so there is no build #3 of it")
        })
        .await;
    assert!(
        session
            .url()
            .await
            .ends_with("/packages/add#no-such-package"),
        "a build of a missing package should offer adding the package: {}",
        session.url().await
    );

    session.open("/worker/nobody-here").await;
    session
        .wait_until("the fleet to explain itself", |t| {
            t.contains("No worker is called nobody-here")
        })
        .await;
    assert!(
        session.url().await.ends_with("/workers"),
        "a missing worker should fall back to the fleet: {}",
        session.url().await
    );

    // The notice belongs to the page it explained, not to whatever comes next.
    session.click_labelled("a", "Packages").await;
    session
        .wait_until("the packages list", |t| t.contains("Upstream"))
        .await;
    assert!(
        !session.text().await.contains("No worker is called"),
        "a notice should not follow you off the page it explained"
    );
}

/// The Reset menu's two ways back, and the patch a save leaves behind.
///
/// "Revert to saved version" and "Revert to upstream" are the same textarea
/// assignment with different sources, so only a change they undo tells them
/// apart -- and a textarea's contents are a property, not markup. Saving a
/// change is also the only way to reach a patch that applies, which is when
/// "View patch" used to be hidden. Upstream is restored and saved at the end,
/// so no patch is left behind for the scenarios after.
async fn a_source_edit_can_be_reset_either_way_and_its_patch_read(session: &Session) {
    const EDITOR: &str = "textarea";
    const MARK: &str = "# edited by the interaction tests";
    let open_editor = || async {
        session.open("/package/hello/source/PKGBUILD").await;
        session
            .wait_for_script(
                "the PKGBUILD to load",
                "return (document.querySelector('textarea')?.value ?? '').includes('pkgname');"
                    .to_string(),
            )
            .await;
    };

    open_editor().await;
    let upstream = session.value_of(EDITOR).await;
    assert_eq!(
        session.count("[role=menu]").await,
        0,
        "the menu starts closed"
    );

    // Unsaved edit, then back to what was saved.
    session
        .type_into(EDITOR, &format!("{upstream}\n{MARK}\n"))
        .await;
    session.click_labelled("button", "Reset ▾").await;
    session.wait_for("[role=menu]").await;
    session
        .click_labelled(
            "[role=menu] button",
            "Revert to saved versionDiscard the edits made since opening it",
        )
        .await;
    assert_eq!(session.value_of(EDITOR).await, upstream, "revert to saved");
    assert_eq!(
        session.count("[role=menu]").await,
        0,
        "choosing closes the menu"
    );

    // Save a change, and it shows as a patch that can be read.
    assert!(
        !session.text().await.contains("View patch"),
        "an unpatched file has no patch to view"
    );
    session
        .type_into(EDITOR, &format!("{upstream}\n{MARK}\n"))
        .await;
    session.click_labelled("button", "Save").await;
    session
        .wait_until("the save to land on the package page", |t| {
            // The patch is stored before the server re-resolves dependencies,
            // and a test server without a working PKGBUILD parser fails only
            // the latter; either way the patch is what the rest checks.
            !t.contains("Reset ▾") || t.contains("Patch saved")
        })
        .await;
    open_editor().await;
    assert!(
        session.value_of(EDITOR).await.contains(MARK),
        "the saved edit reloads"
    );
    session.click_labelled("button", "View patch").await;
    session
        .wait_until("the stored patch to show", |t| {
            t.contains(&format!("+{MARK}"))
        })
        .await;
    session
        .click_labelled(".modal-action button", "Close")
        .await;

    // Back to upstream, from the saved change, and save that to drop the patch.
    session.click_labelled("button", "Reset ▾").await;
    session
        .click_labelled(
            "[role=menu] button",
            "Revert to upstreamDrop every local change; saving then removes the patch",
        )
        .await;
    assert_eq!(
        session.value_of(EDITOR).await,
        upstream,
        "revert to upstream"
    );
    session.click_labelled("button", "Save").await;
    session
        .wait_until("the revert to save", |t| {
            !t.contains("Reset ▾") || t.contains("Patch saved")
        })
        .await;
    open_editor().await;
    assert!(
        !session.text().await.contains("View patch"),
        "saving upstream content removes the patch"
    );
}

/// Opening "Add package" puts the cursor in its search field, so typing
/// starts at once without a click first.
///
/// Only a live page shows this: the `autofocus` attribute fires on a full
/// page load but not when client-side navigation opens the dialog, which is
/// why the field focuses itself on mount instead. Markup alone renders
/// identically either way.
async fn the_add_dialog_focuses_its_search(session: &Session) {
    session.open("/packages/add").await;
    session.wait_for(".modal.modal-open input").await;
    session
        .wait_for_script(
            "the search field to take focus",
            "return document.activeElement instanceof HTMLInputElement;".to_string(),
        )
        .await;
}

/// Stop opens a dialog instead of stopping, and backing out of it stops
/// nothing.
///
/// A stopped build is hours of work gone, and the button sits beside Copy and
/// Follow. The route check sees the dialog's markup; only a click shows that
/// Stop opens it rather than firing, and that "Keep building" closes it without
/// sending the cancel.
async fn stopping_a_build_asks_first(session: &Session) {
    session
        .open("/package/visual-studio-code-bin/build/1")
        .await;
    session
        .wait_until("the running build to load", |t| t.contains("so far"))
        .await;
    assert_eq!(session.count(".modal.modal-open").await, 0);

    session.click_labelled("button", "Stop").await;
    session.wait_for(".modal.modal-open").await;
    session
        .wait_until("the dialog to say what stopping does", |t| {
            // Running or still queued, depending on what earlier scenarios did;
            // both wordings end the same way.
            t.contains("Stop visual-studio-code-bin build #1?") && t.contains("ends as canceled")
        })
        .await;

    session
        .click_labelled(".modal-open .modal-action button", "Keep building")
        .await;
    session
        .wait_for_script(
            "the dialog to close",
            "return document.querySelector('.modal.modal-open') === null;".to_string(),
        )
        .await;
    // Still running, and no cancel went out: the button would read "Stopping…".
    let text = session.text().await;
    assert!(
        text.contains("so far") && !text.contains("Stopping…"),
        "backing out of the dialog stopped the build. Page was:\n{text}"
    );
}

/// Every body row must have exactly as many cells as the table has headers.
///
/// Nothing else catches a misaligned column. A unit test cannot render a
/// component, and the route checks only assert that some text appears
/// *somewhere* on the page -- so when a `td` was replaced rather than added,
/// every column after it shifted one to the left, the last one rendered empty,
/// and both suites stayed green while the page was visibly wrong.
async fn every_row_has_a_cell_for_every_column(session: &Session) {
    for path in ["/builds", "/packages", "/workers"] {
        session.open(path).await;
        session.wait_for("table tbody tr").await;
        // Reports "ok" explicitly rather than an empty string on success:
        // `eval` yields `Default` when a script fails, so an assertion of
        // "nothing was reported" would pass for a check that never ran.
        let report: String = session
            .eval(
                "const t = document.querySelector('table');
                 if (!t) { return 'no table'; }
                 const cols = t.querySelectorAll('thead th').length;
                 const rows = [...t.querySelectorAll('tbody tr')];
                 if (!cols || !rows.length) { return `nothing to check: cols=${cols} rows=${rows.length}`; }
                 const bad = rows
                     .map((r, i) => [i, r.querySelectorAll('td').length])
                     .filter(([, n]) => n !== cols);
                 return bad.length ? `${cols} headers but rows ${JSON.stringify(bad)}` : 'ok';"
                    .to_string(),
            )
            .await;
        assert_eq!(report, "ok", "{path}: row cells do not match header count");
    }
}

/// Typing in the filter narrows the list *and* updates the address bar.
///
/// Both halves are invisible to a rendering test: a box bound one way still
/// shows what was typed, and the URL is not part of the markup.
async fn filtering_narrows_the_list_and_updates_the_url(session: &Session) {
    session.open("/packages").await;
    session
        .wait_until("the list to load", |t| t.contains("neofetch"))
        .await;

    session.type_into("input[type=search]", "neofetch").await;

    session
        .wait_until("the list to narrow", |t| {
            t.contains("neofetch") && !t.contains("visual-studio-code-bin")
        })
        .await;

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if session.url().await.ends_with("#neofetch") {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "URL never picked up the search: {}",
            session.url().await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The stored `makepkg.conf` reaches the editor.
///
/// Not checkable from a DOM dump: a textarea's contents are a property, not
/// serialised markup, so an editor that renders but never fills looks
/// identical there.
async fn a_stored_config_file_is_loaded_into_the_editor(session: &Session) {
    session.open("/settings/config-files").await;
    session.wait_for("textarea").await;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let value = session.value_of("textarea").await;
        if value.contains("MAKEFLAGS") {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the seeded makepkg.conf never reached the editor; textarea held {value:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A search in the URL is applied on arrival, which is what makes one
/// shareable. The counterpart to the scenario above.
async fn a_linked_search_arrives_applied(session: &Session) {
    session.open("/packages#neofetch").await;
    session
        .wait_until("the linked search to apply", |t| {
            t.contains("neofetch") && !t.contains("visual-studio-code-bin")
        })
        .await;
}

/// A status set in the URL is applied on arrival, which is what makes the
/// dashboard's stuck-queue card land on Builds with both queued states
/// ticked. The counterpart to the search scenario above.
async fn a_linked_status_filter_arrives_applied(session: &Session) {
    session.open("/builds?s=enqueued,waiting").await;
    session
        .wait_until("the queue filter to apply", |t| {
            t.contains("2 statuses")
                && t.contains("visual-studio-code-bin")
                && !t.contains("aewm++")
        })
        .await;
    assert!(
        session.url().await.contains("s=enqueued"),
        "URL lost the filter: {}",
        session.url().await
    );
}

/// Approving a worker moves it out of the queue of machines waiting.
///
/// The approval gate is what the workers page is for, and it is the one flow
/// here that changes server state through a button rather than a form. A
/// rendering test sees the button; only this sees what pressing it does.
async fn approving_a_worker_lets_it_build(session: &Session) {
    session.open("/workers").await;
    session
        .wait_until("the fleet to load", |t| t.contains("new-machine"))
        .await;
    assert!(
        session
            .text()
            .await
            .contains("1 worker is waiting for approval"),
        "the fixture should start with exactly one pending worker"
    );

    session.click_labelled("table button", "Approve").await;

    session
        .wait_until("the approval to land", |t| t.contains("Worker approved"))
        .await;
    // The list refetches, so the notice about waiting machines should be gone
    // rather than merely stale.
    session
        .wait_until("the pending notice to clear", |t| {
            !t.contains("waiting for approval")
        })
        .await;
}

/// What a worker is configured with, and what it is actually running.
///
/// Reached by following the worker from the fleet list, which is the half a
/// route check cannot see: the page loads its own declaration, and the link
/// that gets there has to carry the right worker. What the page has to say is
/// not the value alone but where the value came from -- a `200G` that the
/// machine's own variable asked to be `450 giraffes` reads as correct until the
/// source and the refusal are beside it.
async fn a_workers_settings_say_where_each_value_came_from(session: &Session) {
    session.open("/workers").await;
    session
        .wait_until("the fleet to load", |t| t.contains("builder-01"))
        .await;
    // Not in the list: the settings are a page, not a column.
    assert!(
        !session.text().await.contains("builddir_max_bytes"),
        "the fleet list should not carry a worker's settings"
    );

    // builder-01 sorts first, and it is the one the fixture gives a declaration.
    session.click_labelled("table a", "builder-01").await;

    session
        .wait_until("the declaration to load", |t| {
            t.contains("builddir_max_bytes")
        })
        .await;
    assert!(
        session.url().await.ends_with("/worker/builder-01"),
        "following a worker should land on its own page, named: {}",
        session.url().await
    );

    let panel = session.text().await;
    // Grouped the way the worker grouped them. Matched case-insensitively:
    // the headings are uppercased in CSS, and `innerText` reports what the
    // browser renders rather than what the markup says.
    let headings = panel.to_lowercase();
    assert!(headings.contains("scheduling"), "{panel}");
    assert!(headings.contains("build trees"), "{panel}");
    // The value, and the variable that pinned it -- naming the variable is what
    // makes the panel actionable on the machine.
    assert!(panel.contains("pinned by WORKER_CONCURRENCY"), "{panel}");
    // The refusal, with what stands instead.
    assert!(panel.contains("450 giraffes"), "{panel}");
    assert!(panel.contains("built-in default"), "{panel}");
    // A limit nobody set reads as unset, not as zero.
    assert!(panel.contains("build_memory_max"), "{panel}");
    // The page confirms which machine was opened, identity and all.
    assert!(panel.contains("builder-01"), "{panel}");
    assert!(
        panel.contains("1111111111111111aaaa1111111111111111aaaa1111111111111111aaaa1111"),
        "{panel}"
    );

    // A value set here goes through the real save, and the page then says the
    // worker has not taken it yet -- the fixture's worker never checks in.
    // A pinned setting offers no field to type into: a save would not take.
    assert_eq!(
        session.count("input[name=\"concurrency\"]:disabled").await,
        1,
        "a pinned setting should not be editable here"
    );
    session
        .type_into("input[name=\"build_memory_max\"]", "32G")
        .await;
    session.click_labelled("button", "Save 1 change").await;
    session
        .wait_until("the save to land", |t| t.contains("Saved."))
        .await;
    assert_eq!(
        session.value_of("input[name=\"build_memory_max\"]").await,
        "32G",
        "the saved value should be what the field shows"
    );
    session
        .wait_until("the page to say the worker has not taken it", |t| {
            t.contains("has not picked up the latest save yet")
        })
        .await;

    // Paused from its page, the worker says so and offers the way back.
    session.click_labelled("button", "Stop intake").await;
    session
        .wait_until("the pause to land", |t| {
            t.contains("Intake stopped.") && t.contains("intake stopped")
        })
        .await;
    session.click_labelled("button", "Resume intake").await;
    session
        .wait_until("the worker to take builds again", |t| {
            t.contains("Intake resumed.")
        })
        .await;

    // A name two machines answer to cannot be a page. Nothing here guesses
    // which one was meant -- the retired row and the live one differ in exactly
    // what the reader is trying to tell apart.
    session.open("/worker/replaced-host").await;
    session
        .wait_until("the choice to appear", |t| {
            t.contains("workers call themselves this")
        })
        .await;
    // By href: the entry carries badges and a date beside the fingerprint, so
    // its text is not the fingerprint alone.
    session
        .click("a[href=\"/workers/by-cert/555555555555\"]")
        .await;
    session
        .wait_until("the chosen worker to load", |t| {
            t.contains("builddir_max_bytes")
        })
        .await;
    assert!(
        session
            .url()
            .await
            .ends_with("/workers/by-cert/555555555555"),
        "choosing one should go by certificate: {}",
        session.url().await
    );

    // Back to the fleet, so the scenarios after this one start where they
    // expect to.
    session.click_labelled("a", "Workers").await;
    session
        .wait_until("the fleet to come back", |t| t.contains("builder-arm"))
        .await;
}

/// The log's two filters, which are the half a route check cannot see: those
/// only ask what is present, and a filter is judged by what it takes away.
///
/// Both are applied by the *server* -- the log is the one list too long to
/// fetch whole -- so this also proves the query reaches it and comes back
/// narrowed, rather than the browser hiding rows it already had.
async fn the_log_filters_narrow_what_it_shows(session: &Session) {
    session.open("/logs").await;
    session
        .wait_until("the log to load", |t| t.contains("added package"))
        .await;
    let all = session.text().await;
    assert!(all.contains("no space left on device"), "{all}");
    assert!(all.contains("queued a build of yay (rebuild)"), "{all}");

    // Errors only: the failure stays, the ordinary entries go.
    session
        .select_option("select[aria-label=\"Filter by severity\"]", "error")
        .await;
    session
        .wait_until("the ordinary entries to go", |t| {
            !t.contains("queued a build of yay (rebuild)")
        })
        .await;
    let errors = session.text().await;
    assert!(errors.contains("no space left on device"), "{errors}");
    assert!(
        !errors.contains("a worker stopped answering"),
        "a warning is not an error: {errors}"
    );
    // The filter is in the URL, so a narrowed log can be linked to.
    assert!(
        session.url().await.contains("v=error"),
        "{}",
        session.url().await
    );

    // Warnings and errors: the reaped worker comes back, the rest stays gone.
    session
        .select_option("select[aria-label=\"Filter by severity\"]", "warning")
        .await;
    session
        .wait_until("the warning to appear", |t| {
            t.contains("a worker stopped answering")
        })
        .await;
    assert!(
        !session
            .text()
            .await
            .contains("queued a build of yay (rebuild)"),
        "an ordinary entry is neither"
    );

    // Back to everything, then cut at the last restart: the fixture puts two
    // entries before the server-start row.
    session
        .select_option("select[aria-label=\"Filter by severity\"]", "")
        .await;
    session
        .wait_until("everything to come back", |t| {
            t.contains("queued a build of yay (rebuild)")
        })
        .await;
    assert!(
        session
            .text()
            .await
            .contains("deleted package obsolete-thing"),
        "an entry from before the restart is there to be dropped"
    );

    session
        .click("input[aria-label=\"Since the last restart\"]")
        .await;
    session
        .wait_until("the older entries to go", |t| {
            !t.contains("deleted package obsolete-thing")
        })
        .await;
    let booted = session.text().await;
    // The marker is *this* server's own start row, not the one in the fixture:
    // the suite boots a real backend, and it records its own restart. So "this
    // boot" is everything since the test's server came up.
    assert!(booted.contains("started"), "{booted}");
    // Which includes what this test run itself caused: the worker approved by
    // an earlier scenario was logged, and shows up here. That is the whole loop
    // -- an action in the UI, an entry in the log, found by the filter.
    assert!(
        booted.contains("approved worker new-machine"),
        "an action from this run should be in this boot: {booted}"
    );
    assert!(
        session.url().await.contains("b=1"),
        "{}",
        session.url().await
    );

    // About one package: what names it stays, whatever else goes -- and
    // letting the filter go brings the rest back.
    session.open("/logs?e=pkg:yay").await;
    session
        .wait_until("the log about yay", |t| {
            t.contains("queued a build of yay (rebuild)")
        })
        .await;
    let about = session.text().await;
    assert!(about.contains("publishing yay #7 failed"), "{about}");
    assert!(
        !about.contains("added package hello"),
        "an entry about another package is not about this one: {about}"
    );
    session
        .click("button[aria-label=\"Show the whole log\"]")
        .await;
    session
        .wait_until("the whole log to come back", |t| {
            t.contains("added package hello")
        })
        .await;
    assert!(
        !session.url().await.contains("e="),
        "{}",
        session.url().await
    );

    // From a row: its filter menu offers what the entry names -- here the
    // package of the build that failed to publish, beside the build itself.
    session
        .wait_for_script(
            "the publish failure's row menu",
            "const row = [...document.querySelectorAll('tbody tr')] \
               .find(r => r.textContent.includes('no space left on device')); \
             const button = row && row.querySelector('button[aria-haspopup=\"menu\"]'); \
             if (!button) return false; button.click(); return true;"
                .to_string(),
        )
        .await;
    session
        .click_labelled(
            "button[role=\"menuitem\"]",
            "Only entries about this package: yay",
        )
        .await;
    session
        .wait_until("the log narrowed from the row", |t| {
            t.contains("queued a build of yay (rebuild)") && !t.contains("added package hello")
        })
        .await;
    assert!(
        session.url().await.contains("e=pkg:yay"),
        "{}",
        session.url().await
    );
    session
        .click("button[aria-label=\"Show the whole log\"]")
        .await;

    // By name: one search box over packages and workers; picking a
    // suggestion narrows the log to it.
    session
        .type_into(
            "input[aria-label=\"Search for a package or worker to filter by\"]",
            "builder",
        )
        .await;
    session
        .wait_for_script(
            "the worker suggestion to be picked",
            "const option = [...document.querySelectorAll('button[role=\"option\"]')] \
               .find(o => o.textContent.includes('builder-01') && o.textContent.includes('worker')); \
             if (!option) return false; \
             option.dispatchEvent(new MouseEvent('mousedown', { bubbles: true })); return true;"
                .to_string(),
        )
        .await;
    session
        .wait_until("the log about the worker", |t| {
            t.contains("approved worker") && !t.contains("queued a build of yay (rebuild)")
        })
        .await;
    assert!(
        session.url().await.contains("e=worker:builder-01"),
        "{}",
        session.url().await
    );
    session
        .click("button[aria-label=\"Show the whole log\"]")
        .await;

    // By kind: from a row's funnel, then back to every kind from the select.
    session
        .wait_for_script(
            "the rebuild's row menu",
            "const row = [...document.querySelectorAll('tbody tr')] \
               .find(r => r.textContent.includes('queued a build of yay (rebuild)')); \
             const button = row && row.querySelector('button[aria-haspopup=\"menu\"]'); \
             if (!button) return false; button.click(); return true;"
                .to_string(),
        )
        .await;
    session
        .click_labelled(
            "button[role=\"menuitem\"]",
            "Only entries like this: Build queued",
        )
        .await;
    session
        .wait_until("only queued builds", |t| {
            t.contains("queued a build of yay (rebuild)") && !t.contains("approved worker")
        })
        .await;
    assert!(
        session.url().await.contains("k=build.queued"),
        "{}",
        session.url().await
    );
    session
        .select_option("select[aria-label=\"Filter by kind\"]", "")
        .await;
    session
        .wait_until("every kind back", |t| t.contains("approved worker"))
        .await;
}

/// Queueing two packages and taking one back off again.
///
/// Two, deliberately: every chip's ✕ looks the same and sits in the same place,
/// so a handler capturing the wrong label is invisible in the markup — and with
/// one chip queued, removing "the first" and removing "that one" are the same
/// act. This test slept through exactly that until a mutation caught it.
async fn one_queued_package_can_be_taken_back(session: &Session) {
    session.open("/packages/add").await;

    let entry = ".modal-box input[type=text]";
    for repo in ["one", "two"] {
        let url = format!("https://github.com/user/{repo}.git");
        session.type_into(entry, &url).await;
        assert_eq!(
            session.value_of(entry).await,
            url,
            "typing did not land in the entry field"
        );
        session
            .wait_until("the git fields", |t| t.contains("Subfolder"))
            .await;
        session
            .click_labelled(".modal-box button", "Add to list")
            .await;
        session
            .wait_until("the queued chip", |t| t.contains(&format!("{repo}.git")))
            .await;
    }

    session
        .click("button[aria-label^='Remove https://github.com/user/two.git']")
        .await;

    session
        .wait_until("the second chip to go", |t| !t.contains("two.git"))
        .await;
    assert!(
        session.text().await.contains("one.git"),
        "removing one chip took its neighbour with it"
    );
}

/// A queue of several packages is submitted as one bulk add, the dialog closes
/// at once, and each one's outcome arrives on the progress card.
///
/// The failure this guards against is invisible without a browser and a server:
/// the add is started by a dialog that then unmounts, and the polling is owned
/// by the card that outlives it. A mistake in that handover -- never starting,
/// starting twice, losing the offset, not noticing the job finished -- leaves a
/// spinner in the corner forever while the server has long since finished. Two
/// unreachable remotes are used deliberately: what is being tested is that both
/// outcomes are reported, and a failure is the outcome this fixture can produce
/// without network.
async fn a_queue_is_added_as_one_job(session: &Session) {
    session.open("/packages/add").await;

    let entry = ".modal-box input[type=text]";
    for repo in ["bulk-one", "bulk-two"] {
        let url = format!("https://github.com/user/{repo}.git");
        session.type_into(entry, &url).await;
        session
            .wait_until("the git fields", |t| t.contains("Subfolder"))
            .await;
        session
            .click_labelled(".modal-box button", "Add to list")
            .await;
        session
            .wait_until("the queued chip", |t| t.contains(&format!("{repo}.git")))
            .await;
    }

    session
        .click_labelled(".modal-action button", "Add 2 packages")
        .await;

    // The dialog goes as soon as the work is handed over -- the whole point of
    // the change -- so the rest of the site is usable while the add runs.
    session
        .wait_until("the dialog to close", |t| {
            !t.contains("Type a package name to search the AUR")
        })
        .await;

    // Both are named on the card, so the job's per-package outcomes reached the
    // screen -- not just the first, and not a single collapsed error for the
    // batch. That they arrive at all is what proves the card took over the
    // polling from the dialog that started it.
    session
        .wait_until("both packages to be reported", |t| {
            t.contains("bulk-one.git") && t.contains("bulk-two.git")
        })
        .await;
}

/// The export dialog only warns about secrets once they are asked for.
///
/// Invisible to a render test, which sees one paint: the warning is the point
/// of the checkbox, and a warning that is always on screen is one nobody reads
/// by the time it matters. The download itself is a plain link, so what is
/// checked here is that ticking the box changes what the link offers.
async fn the_export_dialog_warns_only_when_secrets_are_asked_for(session: &Session) {
    session.open("/settings").await;
    session.click_labelled(".card button", "Export…").await;
    session
        .wait_until("the export dialog", |t| t.contains("Include secrets"))
        .await;
    assert!(
        !session.text().await.contains("This file is a credential"),
        "the warning is shown before secrets are asked for"
    );

    session.click("input[aria-label='Include secrets']").await;
    session
        .wait_until("the credential warning", |t| {
            t.contains("This file is a credential")
        })
        .await;

    // And the link now asks for them.
    session
        .wait_for_script(
            "the download link to carry the flag",
            "const a = [...document.querySelectorAll('.modal-open a')] \
                 .find(e => e.getAttribute('href')?.includes('include_secrets=true')); \
             return !!a;"
                .to_string(),
        )
        .await;
}

/// The restore dialog offers a drop target and the three choices an import has
/// to make, and disables the one that would be meaningless.
async fn the_restore_dialog_offers_its_options(session: &Session) {
    session.open("/settings").await;
    session.click_labelled(".card button", "Restore…").await;
    session
        .wait_until("the restore dialog", |t| {
            t.contains("Drop a dump here") && t.contains("Replace everything")
        })
        .await;

    // Nothing chosen yet, so there is nothing to preview or restore.
    session
        .wait_for_script(
            "Preview to be disabled with no file",
            "const b = [...document.querySelectorAll('.modal-open .modal-action button')] \
                 .find(e => e.textContent.trim() === 'Preview'); \
             return !!b && b.disabled;"
                .to_string(),
        )
        .await;

    // Replacing everything leaves the per-package policy with nothing to decide.
    session
        .click("input[aria-label='Replace everything']")
        .await;
    session
        .wait_until("the policy to be explained away", |t| {
            t.contains("Nothing will already be here")
        })
        .await;
}

/// Saving a package's config file writes the package's row, not the server's.
///
/// The failure this exists for is invisible to every other kind of test: pass
/// the scope as `None` and the page still renders correctly, still reports a
/// successful save, and quietly edits the file every other package builds
/// against. Only reading the global file back afterwards can tell.
async fn a_per_package_file_leaves_the_server_wide_one_alone(session: &Session) {
    const MARKER: &str = "# only-for-hello";

    session.open("/package/hello/config-files").await;
    // `hello` seeds nothing of its own, so what loads is the server-wide file.
    session
        .wait_until("the inherited file", |t| t.contains("inherited"))
        .await;

    session.type_into("textarea", MARKER).await;
    session.click_labelled("button", "Save").await;
    session
        .wait_until("the save to land on the package", |t| {
            t.contains("package override")
        })
        .await;

    // The actual assertion. A textarea's contents are a property rather than
    // markup, so this is also the only way to read the file back at all.
    session.open("/settings/config-files").await;
    session.wait_for("textarea").await;
    let global = session.value_of("textarea").await;
    assert!(
        !global.contains(MARKER),
        "a per-package save was written to the server-wide makepkg.conf: {global:?}"
    );
    assert!(
        global.contains("MAKEFLAGS"),
        "the server-wide makepkg.conf lost its seeded contents: {global:?}"
    );
}

/// A build flag is stored, comes back on a fresh load, and can be removed.
///
/// The reload is the point. Adding a chip to a local list renders identically
/// to one that reached the server, and this page is the only way to set flags
/// at all.
async fn a_build_flag_survives_a_reload_and_can_be_taken_off(session: &Session) {
    // Deliberately not the placeholder's own text: a field that reported its
    // placeholder as its value would pass if the two matched.
    const FLAG: &str = "--skipinteg";

    session.open("/package/hello").await;
    session
        .wait_until("the empty flag list", |t| t.contains("No build flags"))
        .await;

    session
        .type_into("input[placeholder='--nocheck']", FLAG)
        .await;
    session.click_labelled("button", "Add").await;
    session
        .wait_until("the flag to appear", |t| t.contains(FLAG))
        .await;

    session.open("/package/hello").await;
    session
        .wait_until("the flag to have persisted", |t| t.contains(FLAG))
        .await;

    session
        .click(&format!("button[aria-label='Remove {FLAG}']"))
        .await;
    session
        .wait_until("the flag to be gone", |t| t.contains("No build flags"))
        .await;
}

/// Removing a package takes it out of the repository.
///
/// `2048.c` because nothing else in this suite or in the route list looks at
/// it, and it has no dependency edges — so it is deleted outright rather than
/// demoted to a dependency, which is the case worth asserting.
async fn removing_a_package_takes_it_out_of_the_list(session: &Session) {
    session.open("/package/2048.c").await;
    session
        .wait_until("the remove section", |t| t.contains("Remove package"))
        .await;

    // The dialog is in the document whether or not it is open, so clicking the
    // confirm button without opening it would "pass" without confirming
    // anything. Everything below is scoped to `.modal-open` for that reason.
    session.click_labelled("button", "Remove package").await;
    session.wait_for(".modal-open").await;
    session
        .wait_until("the confirmation to name the package", |t| {
            t.contains("Remove 2048.c?")
        })
        .await;

    session.click(".modal-open .modal-action .btn-error").await;

    session
        .wait_until("the package to leave the list", |t| !t.contains("2048.c"))
        .await;
    // `/packages?`, not `/packages`: the list spreads its filter and sort into
    // the query, and dioxus writes the `?` even with nothing after it
    // (DioxusLabs/dioxus#5792, fixed by the open #5793). The query itself must
    // still be empty for a default view, which is asserted in the unit tests.
    let url = session.url().await;
    assert!(
        url.trim_end_matches('?').ends_with("/packages"),
        "removal did not land back on the list: {url}"
    );
}

/// The list shows what was asked for; the checkbox adds what was pulled in.
///
/// The half a rendering test cannot reach is the absence: a marker can only
/// assert that something is on the page, and the whole point of the default is
/// that `libfoo` is not.
async fn dependencies_stay_out_of_the_list_until_asked_for(session: &Session) {
    session.open("/packages").await;
    session
        .wait_until("the list to load", |t| t.contains("neofetch"))
        .await;

    let listed = session.text().await;
    assert!(
        !listed.contains("libfoo"),
        "a dependency was listed without being asked for: {listed}"
    );

    // The `label` rather than the `input`: clicking it toggles the checkbox the
    // way a person does, and it is the element carrying the text to match on.
    session.click_labelled("label", "Dependencies (2)").await;
    session
        .wait_until("the dependency to appear", |t| t.contains("libfoo"))
        .await;

    // Revealed, but still distinguishable from a package somebody chose.
    let shown = session.text().await;
    assert!(
        shown.contains("dependency"),
        "a revealed dependency was not marked as one: {shown}"
    );

    // And back, so the next scenario sees the list as it found it. The same
    // control both ways now that it is a checkbox rather than a pair of
    // differently-labelled buttons.
    session.click_labelled("label", "Dependencies (2)").await;
    session
        .wait_until("the dependency to go again", |t| !t.contains("libfoo"))
        .await;
}

/// Paging shows rows the previous page did not, and filtering starts over.
///
/// A rendering test can see the controls but not what they do: a Next button
/// that renders and does nothing looks exactly like one that works, and a page
/// that never resets only shows itself once a filter has been typed.
async fn a_second_page_holds_different_builds(session: &Session) {
    session.open("/builds").await;
    session
        .wait_until("the list to load", |t| t.contains("Page 1 of"))
        .await;

    // Counted, not compared as text: the pager's own label differs between
    // pages, so comparing the rendered page against itself would pass even if
    // the table below it never changed — or never paged at all.
    let on_first = session.count("tbody tr").await;
    assert_eq!(
        on_first, 100,
        "a page should hold exactly PAGE_SIZE rows, got {on_first}"
    );

    session.click("button[aria-label='Next page']").await;
    session
        .wait_until("the second page", |t| t.contains("Page 2 of"))
        .await;

    let on_second = session.count("tbody tr").await;
    assert!(
        on_second > 0 && on_second < 100,
        "the second page should hold the remainder, got {on_second}"
    );

    let second_page = session.text().await;
    assert!(
        second_page.contains("Showing 101"),
        "the second page did not start where the first ended: {second_page}"
    );

    // Filtering has to put you back at the start, or a search run from page 2
    // reports matches it is not showing.
    session.type_into("input[type=search]", "paru").await;
    session
        .wait_until("the filter to reset the page", |t| {
            t.contains("Showing 1\u{2013}") || !t.contains("Page 2 of")
        })
        .await;
}

/// A build is findable by what the row calls it, not only by its package.
async fn a_build_can_be_found_by_the_name_the_list_shows(session: &Session) {
    session.open("/builds").await;
    session
        .wait_until("the list to load", |t| t.contains("hello"))
        .await;

    // `hello/2` is the failed second build the fixture gives it.
    session.type_into("input[type=search]", "hello/2").await;
    session
        .wait_until("the list to narrow to one build", |t| {
            t.contains("hello/2") && !t.contains("neofetch")
        })
        .await;
}

/// Clicking a dependency actually opens it.
///
/// Invisible to every rendering check: both pages render correctly on their
/// own, and the URL updates either way. What broke was that navigating between
/// two `/package/:pkgbase` routes reuses the component, so a fetch that
/// captured the first name never ran again -- the address bar said one package
/// while the page showed the other, with no request in flight to explain it.
async fn following_a_dependency_loads_that_package(session: &Session) {
    // `yay` depends on `hello`, so its dependency list has something to click.
    session.open("/package/yay").await;
    session
        .wait_until("the package to load", |t| t.contains("yay"))
        .await;
    assert!(
        session.text().await.contains("hello"),
        "expected yay's dependency list to name hello"
    );

    session.click_labelled("a", "hello").await;

    // The breadcrumb, not the body: `yay` appears on hello's page too, in its
    // dependents list, so "does the text mention hello" is true either way.
    // The heading is what says which package the page is actually about.
    session
        .wait_until("the dependency's own page", |t| {
            t.contains("Packages/hello")
        })
        .await;
    assert!(
        session.url().await.ends_with("/package/hello"),
        "url did not follow the click: {}",
        session.url().await
    );
}

/// Delayed writes must not clobber newer text or fire twice; a failed reset
/// must preserve the override and its draft.
async fn settings_preserve_edits_in_flight_and_reject_double_submission(session: &Session) {
    let original = session.value_of("textarea").await;
    let submitted = format!("{original}\n# submitted by the interaction test");
    let edited = format!("{submitted}\n# typed while saving");
    assert!(
        session
            .eval::<bool>(
                r#"
        window.__originalSettingFetch = window.fetch.bind(window);
        window.__settingWrites = 0;
        window.__failSettingReset = false;
        window.fetch = async (request, options) => {
            const method = options?.method || request.method || 'GET';
            if (method.toUpperCase() === 'DELETE' && window.__failSettingReset) {
                const response = new Response('reset failed', {status: 500});
                // A fetched response has a URL; reqwest requires it on wasm.
                Object.defineProperty(response, 'url', {value: request.url || request});
                return response;
            }
            const response = await window.__originalSettingFetch(request, options);
            if (method === 'PATCH') {
                window.__settingWrites += 1;
                await new Promise(resolve => setTimeout(resolve, 1500));
            }
            return response;
        };
        return true;
    "#
                .to_string()
            )
            .await
    );
    session.type_into("textarea", &submitted).await;
    assert!(session.eval::<bool>(r#"
        const save = [...document.querySelectorAll('button')].find(b => b.textContent.trim() === 'Save');
        save.click(); save.click(); return true;
    "#.to_string()).await);
    session.type_into("textarea", &edited).await;
    session
        .wait_until("the delayed save", |text| text.contains("Saved."))
        .await;
    assert_eq!(session.value_of("textarea").await, edited);
    assert_eq!(
        session
            .eval::<i64>("return window.__settingWrites;".to_string())
            .await,
        1
    );
    assert!(session.text().await.contains("unsaved"));

    // The successful save triggers a refetch, briefly disabling/remounting
    // controls. Wait for the editable stored file before exercising reset.
    session
        .wait_for_script(
            "the reset control after refetch",
            r#"
        return [...document.querySelectorAll('button')]
            .some(button => button.textContent.trim() === 'Reset' && !button.disabled);
    "#
            .to_string(),
        )
        .await;
    session
        .eval::<bool>("window.__failSettingReset = true; return true;".to_string())
        .await;
    session.click_labelled("button", "Reset").await;
    session
        .wait_until("the failed reset", |text| text.contains("reset failed"))
        .await;
    assert_eq!(session.value_of("textarea").await, edited);
    assert!(session.text().await.contains("stored"));
    session
        .eval::<bool>("window.fetch = window.__originalSettingFetch; return true;".to_string())
        .await;
    session.type_into("textarea", &original).await;
    session.click_labelled("button", "Save").await;
    session
        .wait_until("restoring the seeded file", |text| text.contains("Saved."))
        .await;
}

/// Resolve the second file first, then deliver both a late success and a late
/// failure. Neither may replace the selected file or erase its unsaved draft.
async fn source_editor_ignores_superseded_requests(session: &Session) {
    for fail in [false, true] {
        session.open("/package/hello").await;
        session.wait_for("a[href^='/package/hello/source']").await;
        assert!(
            session
                .eval::<bool>(
                    r#"
            const fetch = window.fetch.bind(window);
            window.__sourceRequests = [];
            window.fetch = (request, options) => {
                const url = new URL(request.url || request, location.origin);
                const reply = (body, status = 200) => {
                    const response = new Response(JSON.stringify(body), {
                        status, headers: {'Content-Type': 'application/json'}
                    });
                    Object.defineProperty(response, 'url', {value: url.href});
                    return response;
                };
                if (url.pathname.endsWith('/source/files')) {
                    return Promise.resolve(reply({files: ['PKGBUILD', 'other.txt'], patched: []}));
                }
                if (url.pathname.endsWith('/source/file')) {
                    return new Promise(resolve => {
                        const path = url.searchParams.get('path');
                        window.__sourceRequests.push((fail = false) => resolve(fail
                            ? reply('obsolete failure', 500)
                            : reply({path, original_content: path + ' content', patched_content: null,
                                patch_error: null, stored_patch: null})));
                    });
                }
                return fetch(request, options);
            };
            return true;
        "#
                    .to_string()
                )
                .await
        );
        session.click("a[href^='/package/hello/source']").await;
        session.wait_for("a[title='PKGBUILD']").await;
        session.click("a[title='PKGBUILD']").await;
        session
            .wait_for_script(
                "first file request",
                "return window.__sourceRequests.length === 1;".into(),
            )
            .await;
        session.click("a[title='other.txt']").await;
        session
            .wait_for_script(
                "second file request",
                "return window.__sourceRequests.length === 2;".into(),
            )
            .await;
        session
            .eval::<bool>("window.__sourceRequests[1](); return true;".into())
            .await;
        session
            .wait_for_script(
                "second file loaded",
                "return document.querySelector('textarea')?.value === 'other.txt content';".into(),
            )
            .await;
        session.type_into("textarea", "unsaved draft").await;
        session
            .eval::<bool>(format!("window.__sourceRequests[0]({fail}); return true;"))
            .await;
        // Allow the resolved promise and Dioxus render to run before inspecting.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(session.value_of("textarea").await, "unsaved draft");
        assert!(!session.text().await.contains("obsolete failure"));
        session.type_into("textarea", "other.txt content").await;
        session
            .eval::<bool>("window.__sourceRequests = []; return true;".into())
            .await;
    }
}
