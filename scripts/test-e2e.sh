#!/usr/bin/env bash
set -euo pipefail

# End-to-end test for the remote-worker architecture.
#
# Brings up the AURCache server + one real privileged build worker (see
# compose/docker-compose.e2e.yaml), then: requests a package, waits for the worker to
# enroll/approve, waits for the build to finish, and finally installs the built
# package from the repo in a throwaway container. Single mode only.
#
#   ./scripts/test-e2e.sh <package> [port] [timeout]

: "${1?Usage: $0 <package> [port] [timeout]}"
PACKAGE="$1"
export AURCACHE_PORT="${2:-8080}"
export AURCACHE_MIRROR_PORT=$((AURCACHE_PORT + 1))
export AURCACHE_WORKER_PORT=$((AURCACHE_PORT + 3))
BUILD_TIMEOUT="${3:-600}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
CLI_BIN="$PROJECT_DIR/backend/target/debug/aurcli"

# The human-facing API is plain HTTP now (worker mTLS lives on its own port), so
# the CLI needs no TLS to talk to it.
export AURCACHE_URL="http://localhost:$AURCACHE_PORT/api"
export AURCACHE_TOKEN="${AURCACHE_TOKEN:-}"

# Overridable so the same harness can drive an alternative topology — see
# scripts/test-e2e-hybrid.sh, which points it at the single-container image.
COMPOSE_FILE="${E2E_COMPOSE_FILE:-$PROJECT_DIR/compose/docker-compose.e2e.yaml}"
# Services that must stay alive for the run to be meaningful. The hybrid image
# runs both roles in one container, so it overrides this to a single name.
E2E_SERVICES="${E2E_SERVICES:-aurcache builder}"
LOG_FILE="$(mktemp -t aurcache-e2e-XXXXXX.log)"

# =============================================================================
# Helpers
# =============================================================================

log() { echo "[$(date '+%H:%M:%S')] $*"; }

# Printed at the start, not only on failure: if the run is being captured and
# truncated, the pointer to the complete log has to appear before the noise.
announce_log() { log "Full container logs will be written to: $LOG_FILE"; }

dc() { docker compose -f "$COMPOSE_FILE" "$@"; }

aurcache_cli() { "$CLI_BIN" "$@"; }

# Fail early with an actionable message instead of an obscure error 3 minutes in.
preflight() {
    local missing=0
    for tool in docker jq curl cargo; do
        command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: '$tool' is required but not installed"; missing=1; }
    done
    docker compose version >/dev/null 2>&1 || { echo "ERROR: 'docker compose' (v2) is required"; missing=1; }
    docker info >/dev/null 2>&1 || { echo "ERROR: cannot talk to the Docker daemon"; missing=1; }
    [ "$missing" -eq 0 ] || exit 1
}


# True if a service container has exited. A dead container will never satisfy
# any wait loop, so polling it for the full timeout just delays the diagnosis.
service_exited() {
    local svc="$1" cid
    cid=$(dc ps -aq "$svc" 2>/dev/null | head -1)
    [ -n "$cid" ] || return 1
    [ "$(docker inspect -f '{{.State.Status}}' "$cid" 2>/dev/null)" = "exited" ]
}

# Abort as soon as either container dies, naming the culprit and its last words.
assert_services_alive() {
    local svc
    for svc in $E2E_SERVICES; do
        if service_exited "$svc"; then
            log "ERROR: container '$svc' exited unexpectedly"
            echo "--- last 40 lines from '$svc' ---"
            dc logs "$svc" 2>&1 | filter_noise | tail -n 40
            dump_logs_on_failure
            exit 1
        fi
    done
}

# The services log at debug, and hyper/h2/rustls emit a frame line per I/O op,
# which buries the handful of lines that explain a failure. Drop that unless
# VERBOSE=1.
filter_noise() {
    if [ "${VERBOSE:-0}" = "1" ]; then
        cat
    else
        grep -viE 'h2::|hyper_util::|hyper::proto|rustls::|framed_read|framed_write|Ping \{|tower::buffer' || true
    fi
}

# AURCache's own build log is what actually explains a failed build; container
# logs mostly show the plumbing around it.
dump_build_log() {
    local build_ref="$1"
    [ -n "$build_ref" ] || return 0
    echo "--- AURCache build log (build $build_ref) ---"
    aurcache_cli builds output "$build_ref" 2>&1 | tail -n 100 || echo "    (build log unavailable)"
}

# Show every build's state, not just the one being waited on. A package stuck
# in waiting-for-deps is almost always waiting on a *dependency* that failed, and
# without this the run reports "waiting-for-deps" and says nothing about why.
dump_build_states() {
    echo "--- builds ---"
    # The CLI already renders status labels, so this script does not re-encode
    # them. Anything it cannot reach is reported by the caller.
    aurcache_cli builds list --limit 20 2>/dev/null || echo "    (unavailable)"
}

# Concise by default: the lines that explain the failure, then where to look for
# everything else. Dumping hundreds of lines of pacman chatter buries the one
# line that matters and gets truncated by whatever is capturing the output.
dump_logs_on_failure() {
    dc logs -t > "$LOG_FILE" 2>&1 || true

    dump_build_states

    local build_ref="${CURRENT_BUILD_REF:-}"
    if [ -n "$build_ref" ]; then
        echo "--- build $build_ref: errors ---"
        aurcache_cli builds output "$build_ref" 2>/dev/null \
            | grep -E "^==> ERROR|error:|failed|Permission denied|No such file" \
            | tail -n 15 \
            || echo "    (no error lines matched; see the full log)"
        echo "--- build $build_ref: last 20 lines ---"
        aurcache_cli builds output "$build_ref" 2>/dev/null | tail -n 20 \
            || echo "    (build log unavailable)"
    fi

    echo "--- service errors ---"
    dc logs 2>&1 | filter_noise | grep -iE "error|panic|fatal" | tail -n 15 \
        || echo "    (none)"

    echo
    echo "    Full logs:      $LOG_FILE"
    echo "    Full build log: $CLI_BIN builds output ${build_ref:-<pkgbase>/<number>}"
    echo "    Containers are left running; inspect with:"
    echo "        docker compose -f '$COMPOSE_FILE' logs -f"
    echo "        docker compose -f '$COMPOSE_FILE' exec ${E2E_SERVICES%% *} bash"
    echo "    Tear down with:"
    echo "        docker compose -f '$COMPOSE_FILE' down -v --remove-orphans"
    echo "    Re-run against these containers without rebuilding: REUSE=1 $0 <pkg>"
}

cleanup() {
    local exit_code=$?
    # Cleanup policy:
    #   CLEANUP=1  -> always tear down (even on failure; e.g. CI)
    #   CLEANUP=0  -> never tear down
    #   CLEANUP unset (default): tear down on success, PRESERVE on failure so a
    #                            failed run can be debugged.
    local do_cleanup
    if [ "${CLEANUP:-}" = "1" ]; then
        do_cleanup=1
    elif [ "${CLEANUP:-}" = "0" ]; then
        do_cleanup=0
    elif [ "$exit_code" -eq 0 ]; then
        do_cleanup=1
    else
        do_cleanup=0
    fi

    if [ "$do_cleanup" = "0" ]; then
        echo "=== Leaving containers up (exit $exit_code) ==="
        echo "    Logs: dc logs   (compose file: $COMPOSE_FILE)"
        echo "    Full logs saved to: $LOG_FILE"
        echo "    Clean up with: docker compose -f '$COMPOSE_FILE' down -v --remove-orphans"
        return
    fi

    log "=== Cleaning up ==="
    # Named volumes make cleanup trivial and root-owned-file-proof.
    dc down -v --remove-orphans -t 10 2>/dev/null || true
}

wait_for_service() {
    log "=== Waiting for AURCache API to be ready ==="
    local started; started=$(date +%s)
    for i in $(seq 1 60); do
        if aurcache_cli health > /dev/null 2>&1; then
            echo "    AURCache is ready (after $(($(date +%s) - started))s)"
            return 0
        fi
        # A crashed server (bad image, port clash, migration failure) will never
        # become ready; say so now instead of after the full 120s.
        assert_services_alive
        [ "$i" -eq 60 ] && { log "ERROR: AURCache API not ready after 120s"; return 1; }
        sleep 2
    done
}

wait_for_worker() {
    log "=== Waiting for a worker to enroll and be approved ==="
    local started; started=$(date +%s)
    # OAuth is disabled in this setup, so /api/workers is readable without auth.
    for i in $(seq 1 60); do
        local approved
        approved=$(curl -fsS "http://localhost:$AURCACHE_PORT/api/workers" 2>/dev/null \
            | jq -r '[.[] | select(.status == "approved")] | length' 2>/dev/null || echo 0)
        if [ "${approved:-0}" -ge 1 ]; then
            echo "    Worker approved and connected (after $(($(date +%s) - started))s)"
            return 0
        fi
        assert_services_alive
        [ "$i" -eq 60 ] && { log "ERROR: no worker approved in time"; return 1; }
        sleep 2
    done
}

# =============================================================================
# Steps
# =============================================================================

build_and_start() {
    announce_log

    log "=== Building AURCache CLI ==="
    ( cd "$PROJECT_DIR/backend" && cargo build -q -p aurcache-cli )

    # REUSE=1 keeps whatever is already running: no image rebuild, no teardown,
    # no re-enrollment. Diagnosing a failure usually means changing one variable
    # and re-adding one package, and a full cycle costs minutes for nothing.
    # It deliberately does not rebuild, so it will NOT pick up source changes.
    if [ "${REUSE:-0}" = "1" ] && [ -n "$(dc ps -q aurcache 2>/dev/null)" ]; then
        log "=== Reusing the running stack (REUSE=1; images not rebuilt) ==="
        return
    fi

    log "=== Building images (server + worker) ==="
    dc build

    log "=== Starting services ==="
    dc up -d
}

request_package() {
    log "=== Adding package: $PACKAGE (timeout: ${BUILD_TIMEOUT}s) ==="
    # `pkg add` takes package names positionally and auto-detects git URLs; the
    # old `add aur <name>` / `add git <url>` subcommands are gone. Passing `aur`
    # here made it the *first package name*, so the run died trying to add a
    # nonexistent AUR package called "aur".
    #
    # `--wait` rather than a separate `builds watch`, because the two are a
    # race: a build can be queued, run and fail between the add returning and a
    # watcher's first listing, and from that listing a build that failed a
    # second ago is indistinguishable from one that failed last week. `--wait`
    # lists the builds *before* it adds, so everything that appears afterwards
    # is this run's, however fast it failed. It reports the count the same way
    # this script used to — dependency resolution is recursive, so an
    # implausible number of builds shows a mis-resolved tree long before any of
    # them fails. Progress reporting and stall detection are the CLI's for the
    # same reason they always were: this script has no business re-encoding
    # build status codes or re-parsing `waiting_reason`.
    if ! aurcache_cli pkg add "$PACKAGE" --platform x86_64 \
        --wait \
        --wait-timeout "$BUILD_TIMEOUT" \
        --wait-stall-after "${STALL_AFTER:-120}"; then
        # A dead container explains a failure better than the build log does, so
        # check that first.
        assert_services_alive
        CURRENT_BUILD_REF=$(aurcache_cli --format json builds list --limit 20 2>/dev/null \
            | jq -r "[.[] | select(.pkg_name == \"$PACKAGE\")] | max_by(.number) | \"\(.pkg_name)/\(.number)\"" 2>/dev/null || echo "")
        dump_logs_on_failure
        exit 1
    fi

    # A chroot worker measures what each build used on its disk. Off for the
    # legacy container builder, which has no storage pool to measure.
    local usage
    usage=$(aurcache_cli --format json builds list --limit 20 2>/dev/null \
        | jq -c "[.[] | select(.pkg_name == \"$PACKAGE\")] | max_by(.number) | .disk_usage" 2>/dev/null)
    log "    disk usage: ${usage:-none}"
    if [ "${E2E_EXPECT_DISK_USAGE:-1}" = 1 ] && { [ -z "$usage" ] || [ "$usage" = null ]; }; then
        log "ERROR: the build reported no disk usage"
        exit 1
    fi
}

validate() {
    log "=== Validating built package by installing it from the repo ==="
    # Derive the actual compose network name from the running server container so
    # this doesn't depend on the compose project name.
    local net
    net=$(dc ps -q aurcache | head -1 \
        | xargs docker inspect -f '{{range $k,$v := .NetworkSettings.Networks}}{{$k}}{{end}}')
    log "    Using network: $net"
    docker run --rm \
        --network "$net" \
        archlinux:latest \
        sh -e -c '
            # `\$arch` must reach pacman.conf literally for pacman to expand it
            # to the repo`s arch subdirectory. The heredoc delimiter is unquoted,
            # so an unescaped $arch would be eaten by this shell and leave
            # `Server = http://aurcache:8081/`, which has no repo.db.
            cat >> /etc/pacman.conf << EOF
[repo]
SigLevel = Optional TrustAll
Server = http://aurcache:8081/\$arch
EOF
            (
                pacman-key --init
                pacman-key --populate archlinux
                pacman -Syq archlinux-keyring --noconfirm
                pacman -Suq --noconfirm
            ) >/dev/null 2>&1
            echo "Installing '"$PACKAGE"'"
            pacman -S --noconfirm '"$PACKAGE"'
            pacman -Qi '"$PACKAGE"'
        '
    log "=== End-to-end test complete ==="
}

# =============================================================================
# Main
# =============================================================================

trap cleanup EXIT

RUN_STARTED=$(date +%s)

preflight
build_and_start
wait_for_service   || { dump_logs_on_failure; exit 1; }
wait_for_worker    || { dump_logs_on_failure; exit 1; }
request_package
validate

log "=== Total runtime: $(($(date +%s) - RUN_STARTED))s ==="
