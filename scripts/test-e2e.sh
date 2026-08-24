#!/usr/bin/env bash
set -euo pipefail

: "${1?Usage: E2E_MODE=host|dind $0 <package> [port] [timeout]}"
PACKAGE="$1"
export AURCACHE_PORT="${2:-8080}"
export AURCACHE_MIRROR_PORT=$((AURCACHE_PORT + 1))
BUILD_TIMEOUT="${3:-300}"

# We take security very seriously
AUTH_HEADER="Authorization: Basic $(echo -n 'admin:secret' | base64)"

# Build mode: "dind" (default) uses an internal Podman inside a privileged
# container; "host" mounts the host Docker socket instead.
E2E_MODE="${E2E_MODE:-dind}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
export AURCACHE_URL="http://localhost:$AURCACHE_PORT/api"
export AURCACHE_TOKEN="${AURCACHE_TOKEN:-}"

# A clean slate for each new test.
export TEMP_DIR=$(mktemp -d)
LOG_FILE="$TEMP_DIR/e2e-full.log"
echo "Using temp dir $TEMP_DIR (mode: $E2E_MODE)"
echo "Full service logs (all containers, all output) will be saved to: $LOG_FILE"

# These are mounted by docker-compose
BUILD_DIR="$TEMP_DIR/builds"

# These will be picked up by docker-compose

# =============================================================================
# Helper Functions
# =============================================================================

log() {
    echo "[$(date '+%H:%M:%S')] $*"
}

# Dumps full multi-service logs (with timestamps) to $LOG_FILE for later
# inspection, and prints only the aurcache service's own log lines to the
# console (the ones almost always relevant for triage) so failures aren't
# swamped by noisy registry/build-tool chatter.
dump_logs_on_failure() {
    dc logs -t > "$LOG_FILE" 2>&1 || true
    echo "--- aurcache service logs (tail; full multi-service logs in $LOG_FILE) ---"
    dc logs aurcache 2>&1 | tail -n 200
}


curl_api() {
    local path="$1"
    shift
    curl -s "http://localhost:$AURCACHE_PORT$path" \
        -H "$AUTH_HEADER" \
        -H "Content-Type: application/json" \
        "$@"
}

wait_for_service() {
    log "=== Waiting for AURCache to be ready ==="
    local max_attempts=30
    local delay=2

    for i in $(seq 1 "$max_attempts"); do
        if curl -s "http://localhost:$AURCACHE_PORT/api"  > /dev/null 2>&1; then
            echo "    AURCache is ready"
            return 0
        fi
        if [ "$i" -eq "$max_attempts" ]; then
            return 1
        fi
        sleep "$delay"
    done
}

dc() {
    docker compose -f docker-compose.e2e.$E2E_MODE.yaml "$@"
}

# =============================================================================
# Setup Functions
# =============================================================================

setup_directories() {
    mkdir -p "$TEMP_DIR"/{builds,repo,db,downloads,config/pacman_x86_64}
    chmod 777 "$TEMP_DIR"/{builds,repo,db,downloads}

    # The build config expects mirrorlist at TEMP_DIR/config/pacman_x86_64/mirrorlist
    echo "Server = https://mirror.rackspace.com/archlinux/\$repo/os/\$arch" > "$TEMP_DIR/config/pacman_x86_64/mirrorlist"
}

cleanup() {
    local exit_code=$?
    if [ "$exit_code" -ne 0 ] && [ -z "${CLEANUP:-}" ]; then
        # Auto-preserve on failure unless the caller explicitly set CLEANUP.
        echo "=== Test failed (exit $exit_code): leaving containers/temp dir up for debugging ==="
        echo "    Inspect with: docker compose -f docker-compose.e2e.$E2E_MODE.yaml logs aurcache"
        echo "    Full logs saved to: $LOG_FILE"
        echo "    Temp dir: $TEMP_DIR"
        echo "    When done, clean up with: docker compose -f docker-compose.e2e.$E2E_MODE.yaml down --remove-orphans && rm -rf '$TEMP_DIR'"
        return
    fi

    if [ "${CLEANUP:-1}" = "1" ]; then
        log "=== Cleaning up ==="
        dc down --remove-orphans -t 10 2>/dev/null || true
        # Note: some of the files there were written by root in a docker container.
        # So we're not legally allowed to touch them. But we can use the same docker trick to do that.
        # We need to mount TEMP_DIR's parent to properly remove the folder itself.
        TEMP_PARENT=$(dirname "$TEMP_DIR")
        docker run --rm -v "$TEMP_PARENT:$TEMP_PARENT" archlinux bash -c " rm -rf '$TEMP_DIR' "
    else
        log "=== Skipping cleanup (CLEANUP=0) ==="
    fi
}

start_docker_services() {
    log "=== Starting Docker services ==="
    dc up -d registry
    sleep 2

    log "=== Building and pushing builder image ==="
    docker buildx build --platform linux/amd64 --build-arg TARGETARCH=amd64 --build-arg TARGETPLATFORM=linux/amd64 --build-arg TARGETVARIANT= -q -t localhost:5000/aurcache-builder:test -f docker/builder.Dockerfile --push .

    log "=== Building and starting AURCache ==="
    dc build -q aurcache && dc up -d aurcache
}

configure_aurcache_registry() {
    # Only needed in DinD mode: aurcache runs Podman internally and the
    # registry is reachable by its Docker Compose service name, not localhost.
    if [ "$E2E_MODE" != "dind" ]; then
        return
    fi
    log "=== Configuring AURCache registry ==="
    docker exec -i aurcache-aurcache-1 bash -c "cat > /etc/containers/registries.conf.d/registry.conf" << 'EOF'
[[registry]]
prefix = "registry:5000"
location = "registry:5000"
insecure = true
EOF
}

prepare() {
    start_docker_services

    wait_for_service || { dump_logs_on_failure; exit 1; }

    configure_aurcache_registry
}

# =============================================================================
# Build trigger function
# =============================================================================

request_package() {
    log "=== Adding package: $PACKAGE ==="
    # We're starting from a fresh DB every time, so we know it'll be a new package.
    # If we reused the DB test after test we'd need to delete the package before adding it again.
    local HTTP_STATUS
    local RESPONSE_BODY
    RESPONSE_BODY=$(curl -sS -w '\n%{http_code}' "http://localhost:$AURCACHE_PORT/api/package" \
        -H "$AUTH_HEADER" \
        -H "Content-Type: application/json" \
        -X POST -d "{\"source\": {\"type\": \"aur\", \"name\": \"$PACKAGE\"}, \"platforms\": [\"x86_64\"]}")
    HTTP_STATUS=$(echo "$RESPONSE_BODY" | tail -n1)
    RESPONSE_BODY=$(echo "$RESPONSE_BODY" | sed '$d')
    if [ "$HTTP_STATUS" -lt 200 ] || [ "$HTTP_STATUS" -ge 300 ]; then
        log "ERROR: Package request failed (HTTP $HTTP_STATUS): $RESPONSE_BODY"
        dump_logs_on_failure
        exit 1
    fi
    log "    Package request accepted (HTTP $HTTP_STATUS)"

    log "=== Waiting for build to complete (timeout: ${BUILD_TIMEOUT}s) ==="
    local START_TIME
    START_TIME=$(date +%s)
    while true; do
        local ELAPSED
        ELAPSED=$(($(date +%s) - START_TIME))
        if [ $ELAPSED -gt "$BUILD_TIMEOUT" ]; then
            log "ERROR: Build timed out after ${BUILD_TIMEOUT}s"
            # Show what we can to understand what went wrong.
            dump_logs_on_failure
            exit 1
        fi

        local RESPONSE
        RESPONSE=$(curl_api "/api/packages/list?limit=100")
        local BUILD_STATUS
        BUILD_STATUS=$(echo "$RESPONSE" | jq -r ".[] | select(.name == \"$PACKAGE\") | .status" 2>/dev/null || echo "not_found")

        log "    Build status: $BUILD_STATUS (elapsed: ${ELAPSED}s)"

        case "$BUILD_STATUS" in
            1)  log "    Build completed successfully"; break ;;
            2)  log "ERROR: Build failed"; dump_logs_on_failure; exit 1 ;;
            null|"") log "Package not found yet"; sleep 5 ;;
            *)  sleep 5 ;;
        esac
    done
}


# =============================================================================
# Validation Functions
# =============================================================================

validate() {
    log "=== Validating built package ==="

    # Try to install the package just like a user would.
    docker run --rm \
        --network aurcache_aurcache_network \
        archlinux:latest \
        sh -e -c '
            # First setup the repo we want to test
            cat >> /etc/pacman.conf << EOF
[repo]
SigLevel = Optional TrustAll
Server = http://aurcache-aurcache-1:'${AURCACHE_MIRROR_PORT}'/\$arch
EOF

            echo "Updating test container"
            (
                # Just making sure we are up-to-date
                pacman-key --init
                pacman-key --populate archlinux
                # Need to install this first so we can validate other updates
                pacman -Syq archlinux-keyring --noconfirm
                pacman -Suq --noconfirm
            ) 2>/dev/null >/dev/null

            echo "Installing package"
            pacman -S --noconfirm '$PACKAGE'
            pacman -Qi '$PACKAGE'
        '

    log "=== End-to-end test complete ==="
}

# =============================================================================
# Main
# =============================================================================

trap cleanup EXIT

setup_directories
prepare
request_package
validate
