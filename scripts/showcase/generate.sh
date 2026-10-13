#!/usr/bin/env bash
#
# Builds the showcase snapshot: an instance holding the AUR's most popular
# packages and about a year of believable build history, saved under
# scripts/showcase/snapshot/ so screenshots (and anyone wanting a populated
# instance to look at) can start from it in seconds. restore.sh is the fast
# path; this is the slow one, run only to refresh the data.
#
#   ./scripts/showcase/generate.sh
#
# Nothing is compiled for real. The demo worker publishes a near-empty archive
# for each package's current version, which gives the repository, the artifact
# lists and the dependency graph their real shape; history.py then fills in the
# builds before it from each package's actual version bumps in a local clone of
# the AUR's git mirror, and gives every build a plausible duration, size and
# log.
#
# Needs: cargo, jq, curl, sqlite3, zstd, python3, network access to
# aur.archlinux.org, and a clone of https://github.com/archlinux/aur (one
# branch per pkgbase, ~160k of them) fetched recently enough that the AUR's
# current versions are in it.
#
# One package is built for real, in the e2e stack (a server and a privileged
# chroot worker, as scripts/test-e2e.sh runs): the one the website's
# screenshots show, so the build log on the front page is a genuine one. Its
# latest build takes that build's log, duration, size, memory and disk use.
#
# Env: AUR_GIT (default ~/fetched/aur), N (packages to add, default 200),
# MIN_VOTES (default 50: the AUR's popularity score alone ranks some
# throwaway uploads above real packages), MONTHS (history depth, default 12),
# REAL_PKG (the package built for real, default arch-update; empty for none),
# SHOWCASE_WORKDIR (where the instance is assembled; default under
# ~/.cache, kept afterwards for inspection).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
SNAPSHOT_DIR="$SCRIPT_DIR/snapshot"
TARGET="$PROJECT_DIR/backend/target/debug"

AUR_GIT="${AUR_GIT:-$HOME/fetched/aur}"
N="${N:-200}"
MIN_VOTES="${MIN_VOTES:-50}"
MONTHS="${MONTHS:-12}"
REAL_PKG="${REAL_PKG-arch-update}"
WORKDIR="${SHOWCASE_WORKDIR:-${XDG_CACHE_HOME:-$HOME/.cache}/aurcache/showcase}"
API="http://localhost:8080/api"

log() { echo "[$(date '+%H:%M:%S')] $*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }

for tool in cargo jq curl sqlite3 zstd python3 docker; do
    command -v "$tool" >/dev/null || fail "'$tool' is required"
done
git -C "$AUR_GIT" rev-parse --git-dir >/dev/null 2>&1 \
    || fail "no AUR git mirror at $AUR_GIT (set AUR_GIT)"
port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
for port in 8080 8081 8083; do
    port_busy "$port" && fail "port $port is in use; the server's ports are fixed"
done

SERVER_PID=""
WORKER_PID=""
stop() {
    for pid in $WORKER_PID $SERVER_PID; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    WORKER_PID=""
    SERVER_PID=""
}
trap stop EXIT

log "Building the server, the demo worker, the CLI and the sandbox"
( cd "$PROJECT_DIR/backend" && cargo build --quiet -p aurcache --features aurcache-api/static \
    && cargo build --quiet -p aurcache-worker-demo -p aurcache-cli -p aurcache-sandbox )

log "Assembling the instance in $WORKDIR"
rm -rf "$WORKDIR"
mkdir -p "$WORKDIR/enroll" "$WORKDIR/worker"

if [ -n "$REAL_PKG" ]; then
    # Before anything else here: the stack takes the same fixed ports.
    log "Building $REAL_PKG for real in the e2e stack"
    e2e() { docker compose -f "$PROJECT_DIR/compose/docker-compose.e2e.yaml" "$@"; }
    e2e build --quiet
    e2e down -v --remove-orphans -t 10 >/dev/null 2>&1 || true
    e2e up -d --quiet-pull 2>/dev/null
    for _ in $(seq 1 120); do
        approved=$(curl -fsS "$API/workers" 2>/dev/null \
            | jq '[.[] | select(.status == "approved")] | length' 2>/dev/null || echo 0)
        [ "${approved:-0}" -ge 1 ] && break
        sleep 2
    done
    [ "${approved:-0}" -ge 1 ] || { e2e down -v -t 10; fail "no worker came up in the e2e stack"; }
    real="$WORKDIR/real/$REAL_PKG"
    mkdir -p "$real"
    if ! AURCACHE_URL="$API" "$TARGET/aurcli" pkg add --platform x86_64 --wait \
            --wait-timeout 3600 "$REAL_PKG" > "$real/add.log" 2>&1; then
        e2e down -v -t 10
        fail "$REAL_PKG did not build in the e2e stack; see $real/add.log"
    fi
    number=$(AURCACHE_URL="$API" "$TARGET/aurcli" --format json builds list --package "$REAL_PKG" \
        | jq -r 'map(select(.status == 1)) | max_by(.number) | .number')
    AURCACHE_URL="$API" "$TARGET/aurcli" --format json builds get "$REAL_PKG/$number" > "$real/build.json"
    AURCACHE_URL="$API" "$TARGET/aurcli" builds output "$REAL_PKG/$number" > "$real/output.log"
    curl -fsS "$API/package/$REAL_PKG" > "$real/package.json"
    e2e down -v --remove-orphans -t 10 >/dev/null 2>&1
    log "  built $REAL_PKG #$number in $(jq '.end_time - .start_time' "$real/build.json")s"
fi

# A package whose .SRCINFO does not parse is read from its PKGBUILD instead,
# through the sandbox and the bridge; set up the way CI does, so this works
# on a machine with neither installed.
curl -fsSL -o "$WORKDIR/alpm-pkgbuild-bridge" \
    "https://gitlab.archlinux.org/archlinux/alpm/alpm-pkgbuild-bridge/-/raw/main/alpm-pkgbuild-bridge.sh?ref_type=heads"
chmod 755 "$WORKDIR/alpm-pkgbuild-bridge"

log "Ranking the AUR's packages with at least $MIN_VOTES votes"
meta="$WORKDIR/packages-meta-v1.json.gz"
curl -fsSL -o "$meta" https://aur.archlinux.org/packages-meta-v1.json.gz
# One name per pkgbase: the AUR ranks split packages separately, and adding
# two halves of one base would only add it once.
zcat "$meta" | jq -r --argjson votes "$MIN_VOTES" '
    map(select(.NumVotes >= $votes))
    | group_by(.PackageBase)
    | map(max_by(.Popularity))
    | sort_by(-.Popularity)[]
    | "\(.Name)\t\(.PackageBase)"' > "$WORKDIR/candidates.tsv"

# No VCS packages (`git+` and friends among the sources, whatever the name),
# nor packages that need one: the version check asks their upstream remotes
# whether anything moved, which no snapshot can answer for, so a restored
# instance would find them out of date and start rebuilding them.
VCS_SOURCE='^\s*source(_[a-z0-9_]+)? = ([^:]*::)?(git|svn|hg|bzr|fossil)\+'
vcs_checkouts() {
    grep -lE "$VCS_SOURCE" "$WORKDIR"/source_cache/*/.SRCINFO 2>/dev/null || true
}

start_server() {
    # A year-long version-check interval: its one pass at boot would otherwise
    # be the only thing writing to these packages besides this script.
    ( cd "$WORKDIR" && exec env VERSION_CHECK_INTERVAL=31536000 WORKER_LIVENESS_TIMEOUT=3600 \
        AURCACHE_ENROLLMENT_DIR="$WORKDIR/enroll" AURCACHE_TLS_SANS=localhost \
        AURCACHE_SANDBOX="$TARGET/aurcache-sandbox" \
        AURCACHE_PKGBUILD_BRIDGE="$WORKDIR/alpm-pkgbuild-bridge" \
        "$TARGET/aurcache" >> "$WORKDIR/server.log" 2>&1 ) &
    SERVER_PID=$!
    for _ in $(seq 1 120); do
        curl -fsS "$API/packages/list?limit=1" -o /dev/null 2>/dev/null && return 0
        sleep 1
    done
    tail -20 "$WORKDIR/server.log"
    fail "the server did not come up"
}

start_server

# Dependency resolution needs the official repositories' databases, which the
# server downloads in the background after it starts answering.
log "Waiting for the official repository databases"
for _ in $(seq 1 120); do
    grep -q 'official repository databases are current' "$WORKDIR/server.log" && break
    sleep 1
done
grep -q 'official repository databases are current' "$WORKDIR/server.log" \
    || fail "the official repository databases did not load; see $WORKDIR/server.log"

log "Adding the $N most popular (and what they depend on)"
# One at a time, so a package that does not resolve is skipped rather than
# failing the batch, and one that brought a VCS package with it can be taken
# out again before the next.
: > "$WORKDIR/packages.txt"
added=0
while IFS=$'\t' read -r name base; do
    [ "$added" -ge "$N" ] && break
    # Its own sources, from the mirror: no need to add it to find out.
    if git -C "$AUR_GIT" show "origin/$base:.SRCINFO" 2>/dev/null | grep -qE "$VCS_SOURCE"; then
        continue
    fi
    if ! AURCACHE_URL="$API" "$TARGET/aurcli" pkg add --platform x86_64 "$name" \
            >> "$WORKDIR/add.log" 2>&1; then
        log "  skipped $name (see add.log)"
        continue
    fi
    if [ -n "$(vcs_checkouts)" ]; then
        # Removing the request removes it, with whatever only it needed.
        log "  dropped $name: it needs a VCS package"
        AURCACHE_URL="$API" "$TARGET/aurcli" pkg rm "$base" >> "$WORKDIR/add.log" 2>&1
        for _ in $(seq 1 30); do
            [ -z "$(vcs_checkouts)" ] && break
            sleep 1
        done
        [ -z "$(vcs_checkouts)" ] || fail "removing $name left $(vcs_checkouts | xargs)"
        continue
    fi
    echo "$name" >> "$WORKDIR/packages.txt"
    added=$((added + 1))
done < "$WORKDIR/candidates.tsv"
log "Added $added"

log "Publishing the current versions with the demo worker"
( cd "$WORKDIR/worker" && exec env AURCACHE_URL=https://localhost:8083 \
    AURCACHE_ENROLLMENT_DIR="$WORKDIR/enroll" WORKER_DATA_DIR="$WORKDIR/worker" \
    WORKER_NAME=builder-01 WORKER_CONCURRENCY=16 DEMO_BUILD_SECS=0 \
    "$TARGET/aurcache-worker-demo" >> "$WORKDIR/worker.log" 2>&1 ) &
WORKER_PID=$!
pending() {
    sqlite3 "$WORKDIR/db/db.sqlite" "SELECT count(*) FROM builds WHERE status IN (0, 3, 4, 5)"
}
for _ in $(seq 1 600); do
    [ "$(pending)" = 0 ] && break
    sleep 2
done
[ "$(pending)" = 0 ] || fail "builds still pending after 20 minutes; see $WORKDIR/worker.log"
stop

log "Writing the history"
python3 "$SCRIPT_DIR/history.py" --instance "$WORKDIR" --aur-git "$AUR_GIT" --months "$MONTHS" \
    --real "$WORKDIR/real"
date +%s > "$WORKDIR/taken-at"

log "Saving the snapshot to $SNAPSHOT_DIR"
mkdir -p "$SNAPSHOT_DIR"
sqlite3 "$WORKDIR/db/db.sqlite" .dump | zstd -q -19 -f -o "$SNAPSHOT_DIR/db.sql.zst"
# Each package's AUR files under its pkgbase, without their git history:
# restore.py turns them back into the repositories the server clones from.
# The checkout directory is the source's cache key with anything but
# `[A-Za-z0-9_-]` flattened (`sanitize_cache_key`), hence the lookup.
rm -rf "$WORKDIR/aur"
mkdir "$WORKDIR/aur"
sqlite3 "$WORKDIR/db/db.sqlite" "SELECT name FROM packages" | while read -r name; do
    checkout="$WORKDIR/source_cache/$(printf 'aur_%s' "$name" | tr -c 'A-Za-z0-9_-' '_')"
    [ -d "$checkout" ] || fail "no checkout of $name at $checkout"
    cp -a "$checkout" "$WORKDIR/aur/$name"
    rm -rf "$WORKDIR/aur/$name/.git"
done
# With the published packages, less the official repositories' databases
# (the server downloads those again), and every build's log.
tar -C "$WORKDIR" --exclude=repo/official_repo_cache -cf - repo build_logs aur \
    | zstd -q -19 -T0 -f -o "$SNAPSHOT_DIR/files.tar.zst"
cp "$WORKDIR/taken-at" "$SNAPSHOT_DIR/taken-at"
du -sh "$SNAPSHOT_DIR"/*
log "Done; serve it with ./scripts/showcase/restore.py"
