# shellcheck shell=bash
# The end-to-end harness the e2e scripts share. Sourced, never run.
#
# The caller sets, before sourcing:
#   COMPOSE_ARGS   the `-f FILE...` arguments naming the stack
#   E2E_SERVICES   the services that must stay alive for the run to mean
#                  anything, space-separated
#   AURCACHE_PORT  the host port of the server's API
#   PROJECT_DIR    the checkout
# and may append to, after sourcing:
#   E2E_TOOLS         commands `preflight` requires
#   E2E_KEEP_HINTS    lines the teardown prints when it leaves the stack up
#   E2E_CLEANUP_PATHS paths removed with the stack
#
# Cleanup policy (`trap cleanup EXIT`):
#   CLEANUP=1      always tear down (even on failure; e.g. CI)
#   CLEANUP=0      never tear down
#   CLEANUP unset  tear down on success, preserve on failure so a failed run
#                  can be debugged.

LOG_FILE="$(mktemp -t aurcache-e2e-XXXXXX.log)"
CLI_BIN="$PROJECT_DIR/backend/target/debug/aurcli"
E2E_KEEP_HINTS=()
E2E_CLEANUP_PATHS=()
CURRENT_BUILD_REF=

log() { echo "[$(date '+%H:%M:%S')] $*"; }

dc() { docker compose "${COMPOSE_ARGS[@]}" "$@"; }

aurcache_cli() { "$CLI_BIN" "$@"; }

# `pkg add`, once the server can resolve. A freshly started server serves
# before it has read the official repository databases, and refuses an add
# until then rather than holding it; that one refusal is retried, for up to a
# minute. Anything else, including a failed build under `--wait`, is returned.
pkg_add() {
    local out status attempt
    out=$(mktemp)
    for attempt in $(seq 30); do
        aurcache_cli pkg add "$@" 2>&1 | tee "$out"
        status=${PIPESTATUS[0]}
        if [ "$status" -eq 0 ] || ! grep -q "have not been read yet" "$out"; then
            break
        fi
        log "    the server has not read the repositories yet (attempt $attempt); retrying"
        sleep 2
    done
    rm -f "$out"
    return "$status"
}

# Printed at the start, not only on failure: if the run is being captured and
# truncated, the pointer to the complete log has to appear before the noise.
announce_log() { log "Full container logs will be written to: $LOG_FILE"; }

# Fail early with an actionable message instead of an obscure error 3 minutes in.
# A caller needing more appends to E2E_TOOLS.
E2E_TOOLS=(docker jq curl cargo)
preflight() {
    local missing=0 tool
    for tool in "${E2E_TOOLS[@]}"; do
        command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: '$tool' is required but not installed"; missing=1; }
    done
    docker compose version >/dev/null 2>&1 || { echo "ERROR: 'docker compose' (v2) is required"; missing=1; }
    docker info >/dev/null 2>&1 || { echo "ERROR: cannot talk to the Docker daemon"; missing=1; }
    [ "$missing" -eq 0 ] || exit 1
}

build_cli() {
    log "=== Building AURCache CLI ==="
    (cd "$PROJECT_DIR/backend" && cargo build -q -p aurcache-cli)
}

# True if a service container has exited. A dead container will never satisfy
# any wait loop, so polling it for the full timeout just delays the diagnosis.
service_exited() {
    local svc="$1" cid
    cid=$(dc ps -aq "$svc" 2>/dev/null | head -1)
    [ -n "$cid" ] || return 1
    [ "$(docker inspect -f '{{.State.Status}}' "$cid" 2>/dev/null)" = "exited" ]
}

# Abort as soon as a service dies, naming the culprit and its last words.
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

# The newest build of package `$1`, as `<pkgbase>/<number>`; empty if none.
latest_build_ref() {
    aurcache_cli --format json builds list --limit 20 2>/dev/null \
        | jq -r --arg pkg "$1" \
            '[.[] | select(.pkg_name == $pkg)] | max_by(.number) // empty | "\(.pkg_name)/\(.number)"' \
            2>/dev/null || true
}

# Concise by default: the lines that explain the failure, then where to look for
# everything else. Dumping hundreds of lines of pacman chatter buries the one
# line that matters and gets truncated by whatever is capturing the output.
# CURRENT_BUILD_REF names the build the failure is about, when there is one.
dump_logs_on_failure() {
    dc logs -t >"$LOG_FILE" 2>&1 || true

    # Every build's state, not just the one being waited on. A package stuck in
    # waiting-for-deps is almost always waiting on a *dependency* that failed,
    # and without this the run says nothing about why. The CLI renders the
    # status labels, so this does not re-encode them.
    echo "--- builds ---"
    aurcache_cli builds list --limit 20 2>/dev/null || echo "    (unavailable)"

    local build_ref="$CURRENT_BUILD_REF"
    if [ -n "$build_ref" ]; then
        echo "--- build $build_ref: errors ---"
        aurcache_cli builds output "$build_ref" 2>/dev/null \
            | grep -E "^==> ERROR|error:|failed|Permission denied|No such file|not accessible" \
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
}

cleanup() {
    local exit_code=$?
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
        echo "    Full logs:  $LOG_FILE"
        echo "    Inspect:    docker compose ${COMPOSE_ARGS[*]} logs -f"
        echo "                docker compose ${COMPOSE_ARGS[*]} exec ${E2E_SERVICES%% *} bash"
        echo "    Tear down:  docker compose ${COMPOSE_ARGS[*]} down -v --remove-orphans"
        local hint
        for hint in "${E2E_KEEP_HINTS[@]}"; do echo "    $hint"; done
        return
    fi

    log "=== Cleaning up ==="
    # Named volumes make cleanup trivial and root-owned-file-proof.
    dc down -v --remove-orphans -t 10 >/dev/null 2>&1 || true
    if ((${#E2E_CLEANUP_PATHS[@]})); then rm -rf "${E2E_CLEANUP_PATHS[@]}"; fi
}

wait_for_service() {
    log "=== Waiting for AURCache API to be ready ==="
    local started i
    started=$(date +%s)
    for i in $(seq 1 60); do
        if aurcache_cli health >/dev/null 2>&1; then
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
    local started i approved
    started=$(date +%s)
    # OAuth is disabled in these setups, so /api/workers is readable without auth.
    for i in $(seq 1 60); do
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

# The compose network the server is on, read from its running container so it
# does not depend on the compose project name.
e2e_network() {
    # shellcheck disable=SC2016 # a Go template, not shell
    dc ps -q aurcache | head -1 \
        | xargs docker inspect -f '{{range $k,$v := .NetworkSettings.Networks}}{{$k}}{{end}}'
}
