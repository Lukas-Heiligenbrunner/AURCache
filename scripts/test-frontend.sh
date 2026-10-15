#!/usr/bin/env bash
#
# Renders the Rust frontend in a real browser and asserts each route mounts.
#
# The unit and SSR tests render components directly, so they cannot see whether
# the page actually boots. Every defect of that kind this frontend has had was
# found by opening it: a blank page, a status badge with no background, a table
# that clipped its last column. This is that pass, scripted.
#
# The check is "did the app mount and render this screen", not a pixel
# comparison. Screenshots are captured with --shots for a human to look at;
# they are not asserted on.
#
# Usage:
#   scripts/test-frontend.sh                # smoke check, no network needed
#   scripts/test-frontend.sh --shots out/   # also write screenshots there
#   scripts/test-frontend.sh --online       # include routes that fetch from the AUR
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SEED="$PROJECT_DIR/scripts/fixtures/frontend-seed.sql"

# The API port is compiled in (aurcache_common::ports::AURCACHE_HTTP_PORT), so it
# cannot be moved out of the way.
# One port, because one server. The frontend is embedded into the backend by
# `aurcache-api`'s `static` feature, exactly as it ships, so these checks
# exercise the real asset handler and the real SPA fallback rather than a
# stand-in that reimplements them.
API_PORT=8080

SHOTS=""
ONLINE=0
while [ $# -gt 0 ]; do
    case "$1" in
        --shots) SHOTS="${2:?--shots needs a directory}"; shift 2 ;;
        --online) ONLINE=1; shift ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

CHROME="${CHROME:-}"
if [ -z "$CHROME" ]; then
    for candidate in google-chrome-stable google-chrome chromium chromium-browser; do
        if command -v "$candidate" >/dev/null 2>&1; then CHROME="$candidate"; break; fi
    done
fi

fail() { echo "FAIL: $*" >&2; exit 1; }

# --- preflight -------------------------------------------------------------
# All of this is cheaper to check now than to debug from a blank screenshot.

[ -n "$CHROME" ] || fail "no chrome/chromium found; set CHROME=/path/to/browser"
command -v sqlite3 >/dev/null 2>&1 || fail "sqlite3 not installed"
rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown \
    || fail "rust target wasm32-unknown-unknown not installed"

# A server left running from an earlier run answers on these ports and the whole
# suite then silently tests *that* build. This has happened; refuse instead.
port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
port_busy "$API_PORT" && fail "port $API_PORT is already in use — stop the other server first"

WORKDIR="$(mktemp -d)"
# Every browser launch below gets its own profile directory rather than the
# user's default one. Dozens of launches in quick succession against a shared
# profile interfere with each other -- the symptom is a page that reports as
# mounted while its content is missing, on a different route each run, which is
# indistinguishable from a real rendering bug until you notice the set moves.
CHROME_PROFILE="$WORKDIR/chrome-profile"
BACKEND_PID=""
cleanup() {
    [ -n "$BACKEND_PID" ] && kill "$BACKEND_PID" 2>/dev/null || true
    # Wait for it to release the port, or an immediate rerun trips the
    # busy-port preflight.
    [ -n "$BACKEND_PID" ] && wait "$BACKEND_PID" 2>/dev/null || true
    # `--dump-dom` normally exits on its own, but a launch that does not leaves
    # its process behind, and this script starts one per check. Across repeated
    # runs they accumulate until there is no memory left for the next one --
    # which surfaces as routes failing to render, a different set each time, and
    # reads exactly like a rendering bug in the app. Matched on this run's own
    # profile directory so nothing else's browser is touched.
    pkill -f "user-data-dir=$CHROME_PROFILE" 2>/dev/null || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

# --- build -----------------------------------------------------------------

# One build. `aurcache-api`'s build script compiles the frontend to wasm and
# embeds it, and re-runs whenever the frontend changes, so these checks cannot
# run against a stale bundle -- which they silently did, twice, back when the
# two were separate steps.
echo "==> building the server, frontend and all"
( cd "$PROJECT_DIR/backend" && cargo build --quiet -p aurcache --features aurcache-api/static )

echo "==> starting the server"
# A long interval keeps the scheduler's second pass outside this run. Its first
# pass still happens at boot, which is why the fixture is applied afterwards:
# it sees an empty table and rewrites nothing.
# `exec` so the subshell is *replaced* by the server: without it $! is the
# subshell's pid, killing that leaves the server running, and the next run
# aborts on a busy port.
# A long liveness timeout so "is this worker connected" does not depend on how
# far into the run the check happens: the fixture's online worker last checked
# in seconds before seeding, and the default timeout is 60.
( cd "$WORKDIR" && exec env VERSION_CHECK_INTERVAL=86400 WORKER_LIVENESS_TIMEOUT=3600 \
    "$PROJECT_DIR/backend/target/debug/aurcache" > "$WORKDIR/server.log" 2>&1 ) &
BACKEND_PID=$!

for _ in $(seq 1 120); do
    curl -sf "http://localhost:$API_PORT/api/packages/list?limit=1" -o /dev/null && break
    sleep 1
done
curl -sf "http://localhost:$API_PORT/api/packages/list?limit=1" -o /dev/null \
    || { tail -20 "$WORKDIR/server.log"; fail "server did not come up"; }

echo "==> seeding fixture data"
sqlite3 "$WORKDIR/db/db.sqlite" < "$SEED"

curl -sf "http://localhost:$API_PORT/" -o /dev/null \
    || { tail -20 "$WORKDIR/server.log"; fail "server is not serving the frontend"; }

# --- checks ----------------------------------------------------------------
#
# Each entry is  route|marker|description.
#
# `marker` is something only that screen renders, so the check proves the right
# screen mounted rather than merely that *a* page loaded.
#
# Routes two or more segments deep are the important ones and must stay in this
# list. The relative-asset-path bug that shipped blank pages left `/` and
# `/builds` working — at one segment deep a relative specifier still resolves
# correctly — and broke only `/package/hello/build/1` and below. A list of shallow routes
# would have passed straight through it.
ROUTES=(
    "/|Builds per month|dashboard"
    # The tiles that link somewhere only render with a router above them, which
    # the unit tests cannot stand up — so this is where they are covered.
    "/|Repository|the dashboard shows its headline numbers"
    "/|href=\"/packages?\"|a counted tile links to what it counts"
    # Dependencies are counted apart from what was asked for. The fixture has
    # 10 requested and 2 dependency-only packages; both tiles are links, so
    # this is the only place they render.
    "/|12 with dependencies|dependencies are counted apart from requests"
    # A bad week must not hide inside a good lifetime figure.
    "/|this week|the weekly reading sits beside the lifetime one"
    # The chart is SVG, so a real element proves it drew rather than errored.
    "/|<svg|the build graph renders"
    # The eight dashboard cards below the chart. With the fixture's mix of
    # failed, out-of-date, queued and warning rows, every card renders full
    # rather than collapsed.
    "/|Recent packages|the dashboard shows recent packages"
    "/|Recent builds|the dashboard shows recent builds"
    "/|Failed packages|the dashboard shows failed packages"
    "/|Out of date|the dashboard shows out-of-date packages"
    "/|Stuck queue|the dashboard shows the stuck queue"
    "/|Recent problems|the dashboard shows recent problems"
    "/|Largest packages|the dashboard shows largest packages"
    "/|Longest builds|the dashboard shows longest builds"
    # The stuck queue's title lands on Builds with both queued states in
    # the URL; the interaction suite asserts it arrives applied.
    "/|s=enqueued|the stuck queue links to both queued states"
    "/builds|Duration|builds list"
    "/settings|Max artifact size|the artifact size limit is a setting"
    "/builds|6.4 GiB|a build's peak memory is reported"
    # The fixture holds more builds than fit on a page. That the second page
    # holds different rows is an interaction test; this is only that the
    # controls are there and say where you are.
    "/builds|Page 1 of|a long list is paged"
    "/builds|Showing 1|the pager says which rows are on screen"
    "/package/paru/builds|Page 1 of|a long history is paged too"
    "/package/hello/build/1|hello|build log (depth 4)"
    # A finished build reports its recorded total; a running one (the fixture
    # gives visual-studio-code-bin a start but no end) measures to now.
    "/package/hello/build/1|took 43s|a finished build reports its total duration"
    "/package/hello/build/1|chroot 3.0 GiB · workdir 40 MiB · sources 1.0 MiB|a finished build reports its disk usage"
    "/package/visual-studio-code-bin/build/1|so far|a running build's duration is measured to now"
    # Stop asks first. Whether the dialog opens on click is an interaction
    # test; this is only that a running build renders one to open.
    "/package/visual-studio-code-bin/build/1|Keep building|a running build's Stop asks for confirmation"
    "/packages|Upstream|packages list"
    "/builds|Add package|the sidebar offers adding a package from any page"
    "/packages|Size|the packages list has a size column"
    "/packages|1.2 MiB|the size column totals a package's artifacts"
    "/packages|512 KiB|a successful package reports the size of what it built"
    "/builds|Size|the builds list has a size column"
    "/builds|512 KiB|a successful build reports the size of its output"
    "/packages|—|missing version shows a placeholder"
    # The list is what somebody asked for; the closure is behind a toggle that
    # says how much it is hiding. Whether it actually hides them is an
    # interaction test -- a marker can only assert presence.
    "/packages|Dependencies (2)|dependencies are behind a checkbox"
    # One label per state, because one label for all of them said the same
    # thing of a package that is fine, one that failed and one already queued.
    # That a queued package offers nothing is a unit test -- a marker can only
    # assert presence.
    "/packages|>Rebuild<|a healthy package offers a rebuild"
    "/packages|>Retry<|a failed package offers a retry"
    "/packages|>Update<|an out-of-date package offers an update"
    "/packages/add|AUR package name or git URL|the add dialog (depth 2)"
    # A search in the URL is applied on arrival, so a link to one opens with it
    # already narrowed rather than showing everything.
    # A search in the URL is applied on arrival, so a link to one opens
    # narrowed rather than showing everything. `1 of 10` is the proof it
    # filtered: the name alone would render on the unfiltered page too.
    "/packages#hello|1 of 10|a linked search opens applied"
    "/packages#aewm%2B%2B|1 of 10|a term needing escapes survives the URL"
    "/packages/add|Applied to every package above|one platform choice covers the whole queue"
    # The list stays mounted behind the dialog, so dismissing it reveals the
    # page as it was rather than reloading it.
    "/packages/add|Filter packages|the add dialog leaves the list behind it"
    "/package/hello|Dependencies|package detail (depth 2)"
    "/package/hello|in repo|package detail shows what is in the repo"
    "/package/hello|hello-docs-2.12.1-2-x86_64.pkg.tar.zst|the built artifacts are listed by filename"
    "/package/hello|1.2 MiB|each artifact reports its size"
    "/package/hello|Total|the artifacts add up to a total"
    "/package/neofetch|—|an artifact with no recorded size renders as unknown"
    "/package/hello|Rebuild|the rebuild button sits with the builds"
    "/package/hello|Change|platforms can be edited where they are shown"
    "/package/hello/builds|Platform|per-package build history (depth 3)"
    # The whole row navigates, so the number must not be styled as the only
    # thing that does. `link-primary` on it is the state being guarded against.
    "/package/hello/builds|cursor-pointer|the build rows are clickable"
    "/package/yay|blocking|a blocked package names what is holding it back"
    "/package/yay|too old|a dependency built to an unsatisfying version"
    "/package/my-tool-git|github.com/example/my-tool|a git package links to its repository"
    "/package/my-tool-git|A tool built straight from git|a git package has metadata from its checkout"
    # Per-package config files. Both states are checked: `hello` inherits
    # everything, `neofetch` holds its own.
    "/package/hello/config-files|Config files|per-package config files (depth 3)"
    # Unset here does not mean the builder's own copy -- it means the
    # server-wide file -- and the badge has to say which.
    "/package/hello/config-files|>inherited<|an unset file says it inherits"
    "/package/neofetch/config-files|>package override<|a package that holds its own file says so"
    # Platforms, flags and removal live on the package page itself now.
    "/package/neofetch|--noconfirm|build flags render as chips on the package page"
    "/package/hello|No build flags|a package without flags says so"
    "/package/hello|href=\"/package/hello/config-files\"|the package links to its config files"
    # The one irreversible action, in its own card rather than in the header.
    "/package/hello|Remove package|a package can be removed from its own page"
    "/package/hello|Max artifact size|a package can have its own artifact size limit"
    "/package/hello|>Edit sources<|an unpatched package's sources button says nothing more"
    # The remove card knows which way the delete will go: `yay` needs hello, so
    # it says removing only unflags it, while nothing depends on `paru`.
    "/package/hello|This package has dependents|a package with dependents is told removing only unflags it"
    "/package/paru|Nothing depends on it|a package with no dependents is told removing deletes it"
    # Summed over every version and architecture the package has produced
    # (1200 + 34 + 99), and not over `hello-world`, which a prefix match on the
    # name would have swallowed.
    "/settings|Version check interval|settings"
    # The fixture server runs with VERSION_CHECK_INTERVAL set, so this row is
    # env-locked. Naming the variable proves the source made it all the way
    # from the API to the screen, and the wording has to say what to do about
    # it — the field is disabled and the variable is not set from this page.
    "/settings|unset \$VERSION_CHECK_INTERVAL|an env-pinned setting says how to take it back"
    # Named on every row, set or not, so the page documents what a deployment
    # can pin rather than only reporting what it already pinned.
    "/settings|\$JOB_TIMEOUT|settings document their environment variables"
    "/settings|Builds|settings covers every section"
    "/settings|Backup|the settings page offers backup and restore"
    "/settings|Drop a dump here|a dump can be dropped as well as chosen"
    # Seeded as a stored global value, which is the only state offering a Reset.
    "/settings|>Reset<|settings can undo a stored value"
    "/settings|href=\"/settings/config-files\"|the settings page is the way to the config files"
    "/settings/config-files|makepkg.conf|config files"
    # The path before they moved under Settings; a bookmark of it still lands.
    "/config-files|makepkg.conf|the old config files path redirects"
    # That the stored file reaches the editor is checked in the interaction
    # tests instead: a textarea's value is a DOM property, and a dump of the
    # serialised document does not carry it.
    # Stored and unset render differently; only the stored one offers a Reset.
    "/settings/config-files|>stored<|a stored file says so"
    "/workers|builder-01|workers"
    # The affinity column resolves its entries: a reservation naming a package
    # becomes a link to it, one naming nothing stays plain text.
    "/workers|href=\"/package/visual-studio-code-bin\"|a reservation naming a package links to it"
    "/workers|not-a-package|a reservation naming no package stays plain text"
    # The gate the page exists for: a machine waiting on an operator is called
    # out, not left to be spotted in a status column.
    "/workers|1 worker is waiting for approval|a pending worker is surfaced"
    "/workers|>Approve<|a pending worker can be approved"
    # Emulation is marked rather than listed as native: a fleet that reads as
    # having native aarch64 when it does not is worth not implying.
    "/workers|emulated: armv7h|emulated architectures are marked"
    # Revoked rows are kept so old builds still name their machine, so they are
    # hidden until asked for.
    "/workers|Show retired (2)|retired workers are hidden behind a toggle"
    "/workers|never|a worker that never checked in says so"
    # What the page is for: whether a machine is there, whether it is working,
    # and whether it matters. Approval status cannot answer any of the three --
    # a worker that was approved and then switched off still reads "approved".
    "/workers|1 building|a worker with work in flight says so"
    "/workers|offline|a worker that stopped checking in is marked offline"
    "/workers|% of fleet|a worker's share of the work is shown"
    # The value a machine was configured with is not the one it is running.
    # Flagged in the list, because nobody opens a panel they have no reason to
    # suspect.
    "/workers|refused a configured value|a worker not running its configuration is flagged"
    "/workers|href=\"/worker/builder-01\"|a worker links to its own page by name"
    # A name two machines answer to is linked by certificate instead, so a link
    # from the list never lands on the chooser.
    "/workers|href=\"/workers/by-cert/|a shared name is linked by certificate"
    # The worker page itself: its identity, and the settings it declares.
    "/worker/builder-01|builder-01|one worker"
    "/worker/builder-01|builddir_max_bytes|a worker's declared settings are listed"
    "/worker/builder-01|pinned by WORKER_CONCURRENCY|a value names the variable that set it"
    "/worker/builder-01|450 giraffes|a refused value says what was wrong with it"
    # Two machines have called themselves this, so the name alone is a choice
    # rather than a page.
    "/worker/replaced-host|2 workers call themselves this|a shared name offers a choice"
    "/workers/by-cert/555555555555|builddir_max_bytes|a worker can be reached by its certificate"
    # The package name is a link, so the entry's text is split around it in the
    # DOM: the prose and the link are checked separately rather than as one
    # contiguous string.
    "/logs|added package|logs"
    # The sentence is rendered in the browser from the stored payload, so this
    # also proves the payload shapes in the fixture are ones the catalogue
    # parses.
    "/logs|(rebuild)|the log renders each entry type"
    # Nobody asked for this one; a schedule did. The Dart frontend called that
    # "You", which claims work the reader did not do.
    "/logs|AURCache|an unattributed entry is credited to the server"
    # The log carries what went wrong, not only what people did.
    "/logs|no space left on device|a failure says what went wrong"
    "/logs|a worker stopped answering|a reaped worker is recorded"
    "/logs|AURCache 0.5.0 started|a restart is recorded, with the version"
    "/logs|badge-error|a failure is marked as one"
    # A package an entry is about opens from the log.
    "/logs|href=\"/package/hello\"|an entry links to the package it is about"
    "/logs|href=\"/worker/|a worker entry links to the worker"
    # A build is named by package and number, and opens the build.
    "/logs|href=\"/package/yay/build/7\"|a build entry links to the build"
    # Narrowed to one package, from the link on its page.
    "/logs?e=pkg:hello|added package|the log narrows to one package"
    "/logs?e=pkg:hello|Show the whole log|the log says what it is narrowed to, and lets it go"
    "/package/hello|href=\"/logs?e=pkg:hello\"|a package links to its log"
    # The log can be narrowed from the page, by name or from a row.
    "/logs|Search for a package or worker to filter by|the log offers narrowing by name"
    "/logs|Filter by kind|the log offers narrowing by kind"
    "/logs?k=build.started|Every kind|a kind filter in the URL is applied"
    "/logs|Narrow the log to what this entry is about|each row offers narrowing to what it names"
    # A package's and a worker's own recent entries, on their pages.
    "/package/hello|added package|a package page shows its recent activity"
    "/worker/builder-01|approved worker|a worker page shows its recent activity"
    # A build names both its package and itself, each a link.
    "/logs|href=\"/package/yay\"|a build entry links to its package too"
    # The path before Activity became Logs; a bookmark of it still lands.
    "/activities|added package|the old activities path redirects"
    # Filters ride the query, so a narrowed log is linkable. What a filter
    # *drops* cannot be checked here -- these checks only ask what is present --
    # so the interaction suite covers that.
    "/logs?v=error|no space left on device|a severity filter in the URL is applied"
    "/no/such/page|Not found|404 (depth 3)"
)
if [ "$ONLINE" = "1" ]; then
    # Fetches the PKGBUILD from the AUR, so it needs network. Fetched once here
    # first: a cold fetch outlasts the page's render budget, so whichever check
    # came first saw an editor with no file in it and failed, and only that one.
    curl -sf "http://localhost:$API_PORT/api/package/hello/source/files" -o /dev/null \
        || fail "could not fetch hello's source from the AUR"
    # Both reverts live in the Reset menu, which renders its items only when
    # opened; the interaction tests open it.
    ROUTES+=("/package/hello/source/PKGBUILD|aria-haspopup=\"menu\"|source editor (depth 4)")
    ROUTES+=("/package/hello/source/PKGBUILD|Save &amp; Rebuild|save and queue in one step")
    # The way back is the breadcrumb's package link. It used to be a "← hello"
    # button; this marker went stale when the heading became the trail, and no
    # one noticed because these routes only run with --online.
    ROUTES+=("/package/hello/source/PKGBUILD|href=\"/package/hello\"|the editor offers a way back")
    ROUTES+=("/package/hello/source/PKGBUILD|>Sources<|the editor says where it is")
    # The add dialog offers the same editor for a package that does not exist
    # yet, which is the only way to add one whose PKGBUILD will not parse.
    ROUTES+=("/packages/add#hello|Edit sources before adding|the add dialog can patch one source")
fi

# Things that mean someone was midway through diagnosing something. Deliberately
# narrow: this has to be quiet on real content, and package descriptions are
# arbitrary upstream text.
JUNK='PROBE\[|DEBUG\[|HREF\[|QUERY\[|dbg!|todo!\(|XXXTEMP'

[ -n "$SHOTS" ] && mkdir -p "$SHOTS"

echo "==> checking routes"
failures=0
for entry in "${ROUTES[@]}"; do
    route="${entry%%|*}"; rest="${entry#*|}"
    marker="${rest%%|*}"; desc="${rest#*|}"

    dom="$("$CHROME" --headless --disable-gpu --no-sandbox --user-data-dir="$CHROME_PROFILE" \
             --virtual-time-budget=8000 --dump-dom "http://localhost:$API_PORT$route" 2>/dev/null || true)"

    # The shell is the layout every route renders into. Missing it means the
    # app never mounted -- a blank page -- rather than a wrong screen.
    # A pipe into grep -q can SIGPIPE printf after an early match; with
    # pipefail that falsely reports a missing marker on a large page.
    if ! grep -q "drawer-side" <<<"$dom"; then
        echo "  FAIL  $route ($desc): app did not mount"
        failures=$((failures + 1))
        continue
    fi
    # `--` so a marker that starts with a dash -- a makepkg flag, say -- is
    # read as a pattern rather than as an option to grep.
    if ! grep -q -- "$marker" <<<"$dom"; then
        echo "  FAIL  $route ($desc): mounted, but did not render $marker"
        failures=$((failures + 1))
        continue
    fi

    # Every check above asks whether something is present. None of them can
    # notice something that should not be, which is how a debug probe rendering
    # `HREF[...]QUERY[...]` above two list pages survived four commits: the
    # markers it sat beside still matched. This asks the other question.
    if junk="$(grep -oE "$JUNK" <<<"$dom" | sort -u)" && [ -n "$junk" ]; then
        echo "  FAIL  $route ($desc): left-over debugging in the page: $(printf '%s' "$junk" | tr '\n' ' ')"
        failures=$((failures + 1))
        continue
    fi
    echo "  ok    $route ($desc)"

    if [ -n "$SHOTS" ]; then
        # Routes carry query/fragment characters (`?`, `#`, `=`, `:` …) that
        # `actions/upload-artifact` rejects (NTFS-forbidden `" : < > | * ?`).
        # Keep only a safe allowlist so every shot uploads.
        name="$(printf '%s' "${route#/}" | tr '/' '-' | tr -c 'A-Za-z0-9_.-' '-')"
        [ -z "$name" ] && name="index"
        "$CHROME" --headless --disable-gpu --no-sandbox --user-data-dir="$CHROME_PROFILE" --hide-scrollbars \
            --window-size=1440,1400 --virtual-time-budget=8000 \
            --screenshot="$SHOTS/$name.png" "http://localhost:$API_PORT$route" >/dev/null 2>&1
    fi
done

# Two shots at widths the default 1440 does not cover.
#
# The narrow layout drops columns rather than scrolling them, and that rule has
# broken twice. Settings splits into two columns only above 1440, so the shape
# most people actually see it in is not the shape the run above captured.
if [ -n "$SHOTS" ]; then
    "$CHROME" --headless --disable-gpu --no-sandbox --user-data-dir="$CHROME_PROFILE" --hide-scrollbars \
        --window-size=420,900 --virtual-time-budget=8000 \
        --screenshot="$SHOTS/packages-narrow.png" "http://localhost:$API_PORT/packages" >/dev/null 2>&1
    "$CHROME" --headless --disable-gpu --no-sandbox --user-data-dir="$CHROME_PROFILE" --hide-scrollbars \
        --window-size=1920,1200 --virtual-time-budget=8000 \
        --screenshot="$SHOTS/settings-wide.png" "http://localhost:$API_PORT/settings" >/dev/null 2>&1
    echo "==> screenshots in $SHOTS"
fi

if [ "$failures" -gt 0 ]; then
    fail "$failures route(s) did not render"
fi
echo "==> all ${#ROUTES[@]} routes rendered"

# The checks above dump a page and read it. These drive it: type in a filter,
# queue something, take it back. That needs a session rather than one
# --dump-dom per assertion, so it lives in Rust. The browser is the test
# crate's business — it resolves Chrome, fetches a matching chromedriver and
# stops both — so there is nothing to start or clean up here.
echo "==> checking interactions"
( cd "$PROJECT_DIR/frontend-rs" \
    && AURCACHE_UI="http://localhost:$API_PORT" AURCACHE_ONLINE="$ONLINE" \
       timeout 240 cargo test --quiet --test browser -- --ignored ) \
    || fail "interaction tests failed"
