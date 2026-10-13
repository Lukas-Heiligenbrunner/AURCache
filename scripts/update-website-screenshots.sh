#!/usr/bin/env bash
#
# Refreshes the website's front-page screenshots.
#
# Restores the showcase snapshot (scripts/showcase/: about two hundred popular
# AUR packages and a year of history), serves it with this tree's server, and
# screenshots the dashboard, a package page, a build log and a narrow mobile
# viewport with a headless browser, over docs/static/img/. Run it whenever the
# UI has visibly changed and the front page still shows the old one; it takes
# as long as an incremental server build plus a few seconds.
#
#   ./scripts/update-website-screenshots.sh
#   ./scripts/update-website-screenshots.sh --keep   # leave the server running
#
# Env overrides: SHOT_PKG (the package page and build log shown; default
# arch-update), CHROME (path to a chrome/chromium binary). To refresh the data
# itself rather than the pictures, run scripts/showcase/generate.sh first.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
SHOTS_DIR="$(mktemp -d -t aurcache-shots-XXXXXX)"
INSTANCE="${XDG_CACHE_HOME:-$HOME/.cache}/aurcache/showcase-screenshots"
BASE="http://localhost:8080"

SHOT_PKG="${SHOT_PKG:-arch-update}"
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

CHROME="${CHROME:-}"
if [ -z "$CHROME" ]; then
    for candidate in google-chrome-stable google-chrome chromium chromium-browser; do
        if command -v "$candidate" >/dev/null 2>&1; then CHROME="$candidate"; break; fi
    done
fi

log() { echo "[$(date '+%H:%M:%S')] $*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }

SERVER_PID=""
cleanup() {
    rm -rf "$SHOTS_DIR"
    [ -n "$SERVER_PID" ] || return 0
    if [ "$KEEP" = 1 ]; then
        log "Leaving the server running on $BASE (pid $SERVER_PID)"
        return
    fi
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT

for tool in python3 sqlite3 zstd git cargo; do
    command -v "$tool" >/dev/null || fail "'$tool' is required"
done
[ -n "$CHROME" ] || fail "no chrome/chromium found; set CHROME=/path/to/browser"
(exec 3<>/dev/tcp/127.0.0.1/8080) 2>/dev/null && fail "port 8080 is in use; stop the other server first"

log "Restoring the showcase snapshot and starting the server"
python3 "$PROJECT_DIR/scripts/showcase/restore.py" --dir "$INSTANCE" > "$SHOTS_DIR/restore.log" 2>&1 &
SERVER_PID=$!
until grep -q 'serving on' "$SHOTS_DIR/restore.log"; do
    kill -0 "$SERVER_PID" 2>/dev/null || { cat "$SHOTS_DIR/restore.log"; fail "the restore failed"; }
    sleep 1
done

latest=$(sqlite3 "$INSTANCE/db/db.sqlite" \
    "SELECT max(b.number) FROM builds b JOIN packages p ON p.id = b.pkg_id
     WHERE p.name = '$SHOT_PKG' AND b.status = 1")
[ -n "$latest" ] || fail "the snapshot has no successful build of $SHOT_PKG; pick another SHOT_PKG"

shot() {
    local url="$1" out="$2" width="$3" height="$4"
    "$CHROME" --headless --no-sandbox --disable-gpu --hide-scrollbars \
        --user-data-dir="$SHOTS_DIR/chrome" \
        --window-size="$width,$height" --virtual-time-budget=15000 \
        --screenshot="$out" "$url" 2>/dev/null | tail -n 1
}

log "Screenshotting the dashboard"
shot "$BASE/" "$SHOTS_DIR/screenshot1.png" 2560 1440
log "Screenshotting the $SHOT_PKG package page"
shot "$BASE/package/$SHOT_PKG" "$SHOTS_DIR/screenshot2.png" 2560 1440
log "Screenshotting the $SHOT_PKG #$latest build log"
shot "$BASE/package/$SHOT_PKG/build/$latest" "$SHOTS_DIR/screenshot3.png" 2560 1440
log "Screenshotting a mobile viewport"
shot "$BASE/" "$SHOTS_DIR/screenshot_mobile1.png" 390 844

log "Updating docs/static/img/"
for f in screenshot1 screenshot2 screenshot3 screenshot_mobile1; do
    cp "$SHOTS_DIR/$f.png" "$PROJECT_DIR/docs/static/img/$f.png"
done
( cd "$PROJECT_DIR" && git status --short docs/static/img/ )
log "Done"
