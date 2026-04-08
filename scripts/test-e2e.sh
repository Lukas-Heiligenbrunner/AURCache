#!/usr/bin/env bash
set -euo pipefail

: "${1?Usage: $0 <package> [port] [timeout]}"
PACKAGE="$1"
export AURCACHE_PORT="${2:-8080}"
export AURCACHE_MIRROR_PORT=$((AURCACHE_PORT + 1))
BUILD_TIMEOUT="${3:-300}"

# We take security very seriously
AUTH_HEADER="Authorization: Basic $(echo -n 'admin:secret' | base64)"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
COMPOSE_FILE="$PROJECT_DIR/docker-compose.e2e.yaml"

# A clean slate for each new test.
export TEMP_DIR=$(mktemp -d)
echo "Using temp dir $TEMP_DIR"

# These are mounted by docker-compose
BUILD_DIR="$TEMP_DIR/builds"

# These will be picked up by docker-compose

# =============================================================================
# Helper Functions
# =============================================================================


curl_api() {
    local path="$1"
    shift
    curl -s "http://localhost:$AURCACHE_PORT$path" \
        -H "$AUTH_HEADER" \
        -H "Content-Type: application/json" \
        "$@"
}

wait_for_service() {
    echo "=== Waiting for AURCache to be ready ==="
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
    docker compose -f docker-compose.e2e.yaml "$@"
}

# =============================================================================
# Setup Functions
# =============================================================================

setup_directories() {
    mkdir -p "$BUILD_DIR"/{builds,repo,db,downloads,config/pacman_x86_64}
    chmod 777 "$BUILD_DIR"/{builds,repo,db,downloads}

    # The build config expects mirrorlist at BUILD_DIR/config/pacman_x86_64/mirrorlist
    echo "Server = https://mirror.rackspace.com/archlinux/\$repo/os/\$arch" > "$BUILD_DIR/config/pacman_x86_64/mirrorlist"
}

cleanup() {
    if [ "${CLEANUP:-1}" = "1" ]; then
        echo "=== Cleaning up ==="
        dc down --remove-orphans -t 10 2>/dev/null || true
        # Note: some of the files there were written by root in a docker container.
        # So we're not legally allowed to touch them. But we can use the same docker trick to do that.
        # We need to mount TEMP_DIR's parent to properly remove the folder itself.
        TEMP_PARENT=$(dirname "$TEMP_DIR")
        docker run --rm -v "$TEMP_PARENT:$TEMP_PARENT" archlinux bash -c " rm -rf '$TEMP_DIR' "
    else
        echo "=== Skipping cleanup (CLEANUP=0) ==="
    fi
}

start_docker_services() {
    echo "=== Starting Docker services ==="
    dc up -d registry
    sleep 2

    echo "=== Building and pushing builder image ==="
    docker build -q -t localhost:5000/aurcache-builder:test -f docker/builder.Dockerfile --push .

    echo "=== Building and starting AURCache ==="
    dc build -q aurcache && dc up -d aurcache
}

configure_aurcache_registry() {
    echo "=== Configuring AURCache registry ==="
    echo '[[registry]]
prefix = "localhost"
location = "localhost"
insecure = true' | docker exec -i aurcache-aurcache-1 bash -c "cat > /etc/containers/registries.conf.d/localhost.conf"
}

prepare() {
    start_docker_services

    wait_for_service || { dc logs; exit 1; }

    configure_aurcache_registry
}

# =============================================================================
# Build trigger function
# =============================================================================

request_package() {
    echo "=== Adding package: $PACKAGE ==="
    # We're starting from a fresh DB every time, so we know it'll be a new package.
    # If we reused the DB test after test we'd need to delete the package before adding it again.
    RESPONSE=$(curl_api "/api/package" -X POST -d "{\"source\": {\"type\": \"aur\", \"name\": \"$PACKAGE\"}, \"platforms\": [\"x86_64\"]}")

    # Hofstadter's law: It always takes longer than you expect, even when you take into account Hofstadter's law.
    echo "=== Waiting for build to complete (timeout: ${BUILD_TIMEOUT}s) ==="
    local START_TIME
    START_TIME=$(date +%s)
    while true; do
        local ELAPSED
        ELAPSED=$(($(date +%s) - START_TIME))
        if [ $ELAPSED -gt "$BUILD_TIMEOUT" ]; then
            echo "ERROR: Build timed out after ${BUILD_TIMEOUT}s"
            # Show what we can to understand what went wrong.
            dc logs
            exit 1
        fi

        local RESPONSE
        RESPONSE=$(curl_api "/api/packages/list?limit=100")
        local BUILD_STATUS
        BUILD_STATUS=$(echo "$RESPONSE" | jq -r ".[] | select(.name == \"$PACKAGE\") | .status" 2>/dev/null || echo "not_found")

        echo "    Build status: $BUILD_STATUS (elapsed: ${ELAPSED}s)"

        case "$BUILD_STATUS" in
            1)  echo "    Build completed successfully"; break ;;
            2)  echo "ERROR: Build failed"; dc logs; exit 1 ;;
            null|"") echo "Package not found yet"; sleep 5 ;;
            *)  sleep 5 ;;
        esac
    done
}

# =============================================================================
# Validation Functions
# =============================================================================

validate() {
    echo "=== Validating built package ==="

    # Try to install the package just like a user would.
    docker run --rm \
        --network host \
        archlinux:latest \
        sh -e -c '
            # First setup the repo we want to test
            cat >> /etc/pacman.conf << EOF
[repo]
SigLevel = Optional TrustAll
Server = http://localhost:'${AURCACHE_MIRROR_PORT}'/\$arch
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

    echo "=== End-to-end test complete ==="
}

# =============================================================================
# Main
# =============================================================================

trap cleanup EXIT

setup_directories
prepare
request_package
validate
