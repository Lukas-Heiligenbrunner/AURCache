#!/usr/bin/env bash
set -euo pipefail

# End-to-end test for build credentials reaching the chroot.
#
# A package whose sources sit behind SSH is the motivating case for worker
# package-affinity (`unreal-engine`), and the credential path has one step no
# unit test can cover: whether the key staged for a job is readable by the
# unprivileged user makepkg runs as inside makechrootpkg. That depends on
# devtools' uid mapping, so it needs a real chroot and a real authenticated
# fetch.
#
# Everything here is ephemeral — a keypair generated per run, a throwaway git
# server that authorises it — so this runs in CI wherever Docker does, with no
# secret committed or configured.
#
# Two phases, in order:
#   1. unauthorised — the worker falls back to its own generated key, which the
#      server does not authorise. The build MUST fail. Without this the whole
#      test could pass for the wrong reason (sources cached, fetch skipped, an
#      agent key picked up).
#   2. authorised — the worker is pointed at the key the server accepts. The
#      build must succeed AND the payload's marker must appear in the installed
#      package, proving the fetch really happened.
#
#   ./scripts/test-e2e-ssh.sh [timeout]

BUILD_TIMEOUT="${1:-900}"
export AURCACHE_PORT="${AURCACHE_PORT:-8080}"
export AURCACHE_MIRROR_PORT=$((AURCACHE_PORT + 1))
export AURCACHE_WORKER_PORT=$((AURCACHE_PORT + 3))

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
CLI_BIN="$PROJECT_DIR/backend/target/debug/aurcli"
export AURCACHE_URL="http://localhost:$AURCACHE_PORT/api"
export AURCACHE_TOKEN="${AURCACHE_TOKEN:-}"
export SSH_TEST_MARKER="aurcache-ssh-credential-reached-the-chroot"

COMPOSE=(-f "$PROJECT_DIR/compose/docker-compose.e2e.yaml" -f "$PROJECT_DIR/compose/docker-compose.e2e-ssh.yaml")
dc() { docker compose "${COMPOSE[@]}" "$@"; }
# Progress goes to stderr: `run_phase`'s stdout is captured by the caller, so
# anything logged there would be swallowed into the result.
log() { echo "[$(date '+%H:%M:%S')] $*" >&2; }
cli() { "$CLI_BIN" "$@"; }

KEYDIR="$(mktemp -d -t aurcache-ssh-keys-XXXXXX)"
export SSH_TEST_KEYS="$KEYDIR"

cleanup() {
    local exit_code=$?
    # Match scripts/test-e2e.sh: tear down on success, preserve on failure so a
    # failed run can actually be inspected. CLEANUP=1/0 forces either way.
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
        echo "    Inspect:    docker compose ${COMPOSE[*]} logs -f"
        echo "    Tear down:  docker compose ${COMPOSE[*]} down -v --remove-orphans"
        # The keypair is deliberately kept when preserving state: without it the
        # surviving containers cannot be driven by hand.
        echo "    Keypair:    $KEYDIR  (delete when done)"
        exit $exit_code
    fi

    log "=== Cleaning up ==="
    dc down -v --remove-orphans >/dev/null 2>&1 || true
    rm -rf "$KEYDIR"
    exit $exit_code
}
trap cleanup EXIT

# Concise by default: the lines that explain the failure, then pointers. See the
# same rationale in scripts/test-e2e.sh.
dump_failure() {
    dc logs -t > "$LOG_FILE" 2>&1 || true
    echo "--- build errors ---"
    cli builds output 1 2>/dev/null \
        | grep -E "^==> ERROR|error:|Permission denied|No such file|not accessible" \
        | tail -n 15 \
        || echo "    (no error lines matched)"
    echo "--- worker log (errors) ---"
    dc logs builder 2>&1 | grep -iE "error|warn|denied" | tail -n 10 || echo "    (none)"
    echo
    echo "    Full logs:      $LOG_FILE"
    echo "    Full build log: $CLI_BIN builds output 1"
}

status_name() {
    case "${1:-}" in
        0) echo active ;; 1) echo success ;; 2) echo failed ;;
        3) echo enqueued ;; 4) echo waiting-for-deps ;; *) echo "unknown(${1:-none})" ;;
    esac
}

build_status() {
    curl -fsS "http://localhost:$AURCACHE_PORT/api/builds" 2>/dev/null \
        | python3 -c 'import json,sys
try: d=json.load(sys.stdin)
except Exception: sys.exit()
print(d[0]["status"] if d else "")' 2>/dev/null || true
}

# Bring the stack up and drive one build to a terminal state. Echoes the final
# status name.
run_phase() {
    local phase="$1" keyenv="$2"
    log "=== Phase: $phase ==="
    dc down -v --remove-orphans >/dev/null 2>&1 || true
    # Must be exported, not just prefixed: `VAR=x dc …` sets it for the shell
    # function but does not reliably reach `docker compose` beneath it, which
    # silently made the "authorised" phase identical to the unauthorised one.
    export WORKER_GIT_SSH_KEY="$keyenv"
    dc up -d --build >/dev/null

    log "    waiting for the API"
    for _ in $(seq 1 60); do
        curl -fsS "http://localhost:$AURCACHE_PORT/api/health" >/dev/null 2>&1 && break
        curl -fsS "http://localhost:$AURCACHE_PORT/api/builds" >/dev/null 2>&1 && break
        sleep 2
    done

    log "    waiting for a worker"
    for _ in $(seq 1 60); do
        [ "$(curl -fsS "http://localhost:$AURCACHE_PORT/api/workers" 2>/dev/null \
            | python3 -c 'import json,sys
try: print(sum(1 for w in json.load(sys.stdin) if w["status"]=="approved"))
except Exception: print(0)')" -gt 0 ] && break
        sleep 2
    done

    log "    adding the fixture package"
    cli pkg add "git://gitssh/pkg.git" --ref master >/dev/null

    local waited=0 st
    while [ "$waited" -lt "$BUILD_TIMEOUT" ]; do
        st="$(build_status)"
        case "$st" in
            1|2) echo "$(status_name "$st")"; return 0 ;;
        esac
        sleep 5; waited=$((waited + 5))
    done
    echo "timeout"
}

log "Full container logs will be written to: $LOG_FILE"
log "=== Building the CLI ==="
(cd "$PROJECT_DIR/backend" && cargo build -q -p aurcache-cli)

log "=== Generating a throwaway keypair ==="
ssh-keygen -q -t ed25519 -N '' -C aurcache-ssh-test -f "$KEYDIR/id_ed25519"
# The worker container runs as an unprivileged user whose uid need not match
# the host's, and `mktemp -d` gives 0700 / ssh-keygen gives 0600 — both
# unreadable there. Widen them: this key exists only for the duration of the
# run, and the worker copies it to 0600 in the job workspace anyway, which is
# the same normalisation a root-owned Docker secret needs.
chmod 755 "$KEYDIR"
chmod 644 "$KEYDIR/id_ed25519" "$KEYDIR/id_ed25519.pub"

# ---- Phase 1: the fixture must genuinely require the credential ------------
result="$(run_phase "unauthorised (expect failure)" "")"
if [ "$result" != "failed" ]; then
    log "ERROR: build was '$result' without an authorised key; the test would"
    log "       prove nothing — the source may be cached or the fetch skipped."
    dump_failure
    exit 1
fi
log "    build failed as required"

# ---- Phase 2: with the authorised key it must build ------------------------
result="$(run_phase "authorised (expect success)" "/keys/id_ed25519")"
if [ "$result" != "success" ]; then
    log "ERROR: build was '$result' with the authorised key"
    dump_failure
    exit 1
fi
log "    build succeeded"

# Guard against the phase collapsing again: if the worker fell back to a
# self-generated key, the phases would be identical and a pass would mean
# nothing.
if dc logs builder 2>&1 | grep -q "Build SSH key:"; then
    log "ERROR: the worker generated its own key; WORKER_GIT_SSH_KEY did not"
    log "       reach the container, so this phase proves nothing."
    exit 1
fi

# ---- The marker proves the SSH fetch actually happened ---------------------
log "=== Verifying the payload reached the package ==="
found="$(docker run --rm --network "aurcache_aurcache_network" archlinux:latest bash -c "
  set -e
  printf '[repo]\nSigLevel = Never\nServer = http://aurcache:8081/\$arch\n' >> /etc/pacman.conf
  pacman -Sy --noconfirm >/dev/null 2>&1
  pacman -S --noconfirm aurcache-ssh-test >/dev/null 2>&1
  cat /usr/share/aurcache-ssh-test/marker.txt
" 2>/dev/null || true)"

if [ "$found" != "$SSH_TEST_MARKER" ]; then
    log "ERROR: marker missing from the built package (got: '${found:-<nothing>}')"
    exit 1
fi

log "=== SSH credential test complete ==="
log "    marker: $found"
