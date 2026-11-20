#!/usr/bin/env bash
set -euo pipefail

# End-to-end test for the remote-worker architecture.
#
# Brings up the AURCache server + one real privileged build worker (see
# compose/docker-compose.e2e.yaml), then: requests a package, waits for the worker to
# enroll/approve, waits for the build to finish, and finally installs the built
# package from the repo in a throwaway container.
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

# The human-facing API is plain HTTP (worker mTLS lives on its own port), so
# the CLI needs no TLS to talk to it.
export AURCACHE_URL="http://localhost:$AURCACHE_PORT/api"
export AURCACHE_TOKEN="${AURCACHE_TOKEN:-}"

# Overridable so the same harness can drive an alternative topology — see
# scripts/test-e2e-hybrid.sh, which points it at the single-container image.
COMPOSE_ARGS=(-f "${E2E_COMPOSE_FILE:-$PROJECT_DIR/compose/docker-compose.e2e.yaml}")
# The hybrid image runs both roles in one container, so it overrides this to a
# single name.
E2E_SERVICES="${E2E_SERVICES:-aurcache builder}"

# shellcheck source=scripts/lib/e2e.sh
. "$SCRIPT_DIR/lib/e2e.sh"
E2E_KEEP_HINTS+=("Re-run against these containers without rebuilding: REUSE=1 $0 <pkg>")

# =============================================================================
# Steps
# =============================================================================

build_and_start() {
    announce_log
    build_cli

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
    fresh_worker_data >/dev/null

    log "=== Starting services ==="
    dc up -d
}

request_package() {
    log "=== Adding package: $PACKAGE (timeout: ${BUILD_TIMEOUT}s) ==="
    # `--wait` rather than a separate `builds watch`, because the two are a
    # race: a build can be queued, run and fail between the add returning and a
    # watcher's first listing, and from that listing a build that failed a
    # second ago is indistinguishable from one that failed last week. `--wait`
    # lists the builds *before* it adds, so everything that appears afterwards
    # is this run's, however fast it failed. It reports how many builds the add
    # queued -- dependency resolution is recursive, so an implausible number
    # shows a mis-resolved tree long before any of them fails -- and does the
    # progress reporting and stall detection, so this script never re-encodes
    # build status codes or re-parses `waiting_reason`.
    if ! pkg_add "$PACKAGE" --platform x86_64 \
        --wait \
        --wait-timeout "$BUILD_TIMEOUT" \
        --wait-stall-after "${STALL_AFTER:-120}"; then
        # A dead container explains a failure better than the build log does, so
        # check that first.
        assert_services_alive
        CURRENT_BUILD_REF=$(latest_build_ref "$PACKAGE")
        dump_logs_on_failure
        exit 1
    fi

    # A chroot worker measures what each build used on its disk. Off for the
    # Docker worker, which has no storage pool to measure.
    local usage
    usage=$(aurcache_cli --format json builds list --limit 20 2>/dev/null \
        | jq -c --arg pkg "$PACKAGE" '[.[] | select(.pkg_name == $pkg)] | max_by(.number) | .disk_usage' 2>/dev/null)
    log "    disk usage: ${usage:-none}"
    if [ "${E2E_EXPECT_DISK_USAGE:-1}" = 1 ] && { [ -z "$usage" ] || [ "$usage" = null ]; }; then
        log "ERROR: the build reported no disk usage"
        exit 1
    fi
}

# The chroot worker's network policy, checked against a real build's rules:
# from inside the build's cgroup, which is what the rules match, both root --
# what pacman and a dependency's install script run as -- and the build user
# must be refused the server's worker protocol and API (local addresses here,
# refused like any other), and reach the repository (allowed by the build's
# pacman.conf). The rules exist only while a build runs, so the probe starts
# before the package is added, waits for them, and joins that build's cgroup
# in a cgroup of its own below it. The worker's table is named after it
# (aurcache_worker_<fingerprint>), and is the only one in its container. Its
# builds' cgroups are beside the cgroup it moved its own processes to, PID 1
# among them: in the host's cgroup namespace, which the compose file gives the
# worker, that is below the container's own cgroup. Off for the Docker worker,
# which has no such rules.
PROBE_OUT=
PROBE_PID=
start_build_network_probe() {
    [ "${E2E_CHECK_BUILD_FIREWALL:-1}" = 1 ] || return 0
    PROBE_OUT=$(mktemp)
    dc exec -T -u root builder bash -s "$BUILD_TIMEOUT" >"$PROBE_OUT" 2>&1 <<'PROBE' &
set -u
deadline=$((SECONDS + $1))
probe() {
    local who=$1 port want got status
    for port in 8080:refused 8083:refused 8081:open; do
        want=${port#*:}
        port=${port%:*}
        status=0
        if [ "$who" = root ]; then
            curl -s -o /dev/null --max-time 5 "http://aurcache:$port/" || status=$?
        else
            runuser -u builder -- curl -s -o /dev/null --max-time 5 "http://aurcache:$port/" || status=$?
        fi
        # 7 is a refused connection, which is what the policy's reset gives.
        # Anything else means something answered, if only the worker port's
        # TLS refusing plain HTTP.
        if [ "$status" = 7 ]; then got=refused; else got=open; fi
        echo "check $who aurcache:$port $got want $want"
    done
}
while ((SECONDS < deadline)); do
    table=$(nft list tables inet 2>/dev/null | awk '$3 ~ /^aurcache_worker_/ { print $3; exit }')
    worker=$(sed -n 's/^0:://p' /proc/1/cgroup)
    base=/sys/fs/cgroup$(dirname "$worker")
    for cg in "${base%/}"/builds/build-*/; do
        [ -n "$table" ] && [ -d "$cg" ] || continue
        build=$(basename "$cg")
        nft list chain inet "$table" "$build" >/dev/null 2>&1 || continue
        mkdir -p "$cg/e2e-probe" && echo $$ >"$cg/e2e-probe/cgroup.procs" || continue
        echo "joined $build, in table $table"
        probe root
        probe builder
        exit 0
    done
    sleep 0.2
done
echo "no build's network rules were ever in place"
exit 1
PROBE
    PROBE_PID=$!
}

check_build_network() {
    [ "${E2E_CHECK_BUILD_FIREWALL:-1}" = 1 ] || return 0
    log "=== Checking what a build can reach on the server ==="
    local failed=0 tag who target got want ref
    wait "$PROBE_PID" || failed=1
    while read -r tag who target got _ want; do
        [ "$tag" = check ] || continue
        log "    $target is $got to $who in the build's cgroup (want $want)"
        [ "$got" = "$want" ] || failed=1
    done <"$PROBE_OUT"
    grep -q '^check ' "$PROBE_OUT" || failed=1
    # The build's log says what the policy refused it: here, the probe.
    ref=$(latest_build_ref "$PACKAGE")
    if ! aurcache_cli builds output "$ref" 2>/dev/null | grep -q ':8080/tcp'; then
        log "    the build's log does not report the refused connections"
        failed=1
    fi
    if [ "$failed" = 1 ]; then
        log "ERROR: the build network policy does not hold"
        cat "$PROBE_OUT"
        dc exec -T -u root builder nft list ruleset inet 2>&1 || true
        exit 1
    fi
    rm -f "$PROBE_OUT"
}

validate() {
    log "=== Validating built package by installing it from the repo ==="
    local net
    net=$(e2e_network)
    log "    Using network: $net"
    docker run --rm -i \
        --network "$net" \
        -e PACKAGE="$PACKAGE" \
        archlinux:latest \
        sh -e <<'EOF'
cat >> /etc/pacman.conf <<'CONF'
[repo]
SigLevel = Optional TrustAll
Server = http://aurcache:8081/$arch
CONF
(
    pacman-key --init
    pacman-key --populate archlinux
    pacman -Syq archlinux-keyring --noconfirm
    pacman -Suq --noconfirm
) >/dev/null 2>&1
echo "Installing $PACKAGE"
pacman -S --noconfirm "$PACKAGE"
pacman -Qi "$PACKAGE"
EOF
    log "=== End-to-end test complete ==="
}

# =============================================================================
# Main
# =============================================================================

trap cleanup EXIT

RUN_STARTED=$(date +%s)

preflight
build_and_start
wait_for_service || { dump_logs_on_failure; exit 1; }
wait_for_worker || { dump_logs_on_failure; exit 1; }
start_build_network_probe
request_package
check_build_network
validate

log "=== Total runtime: $(($(date +%s) - RUN_STARTED))s ==="
