#!/usr/bin/env bash
# End-to-end test for the hybrid image's Docker worker.
#
# Covers the upgrade path of a pre-worker deployment that used "host build
# mode": Docker socket mounted, BUILD_ARTIFACT_DIR set, container not
# privileged. That combination selects the Docker worker, which spawns a
# container per package instead of building in a chroot.
#
# Usage: scripts/test-e2e-hybrid-legacy.sh [package] [port] [timeout]
#
# Environment:
#   E2E_BUILD_DIR     the directory the worker shares with the Docker daemon,
#                     by the same absolute path on the host (default
#                     /tmp/aurcache-legacy-builds); every build's tree goes
#                     through it, so name one with room where /tmp is small
#   E2E_COMPOSE_FILE  another compose file of the same shape, for an image
#                     already built or storage somewhere else
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# The builder image the Docker worker spawns. Built locally so the test does
# not depend on a published tag.
docker build -q -f "$PROJECT_DIR/docker/builder.Dockerfile" \
    -t aurcache-legacy-builder:test "$PROJECT_DIR" >/dev/null

# Shared with the Docker daemon by absolute path, so it must exist on the host.
export E2E_BUILD_DIR="${E2E_BUILD_DIR:-/tmp/aurcache-legacy-builds}"
mkdir -p "$E2E_BUILD_DIR"

export E2E_COMPOSE_FILE="${E2E_COMPOSE_FILE:-$PROJECT_DIR/compose/docker-compose.e2e-hybrid-legacy.yaml}"
export E2E_SERVICES="aurcache"
# The container builder has no storage pool, so its builds report no disk use.
export E2E_EXPECT_DISK_USAGE=0
# The Docker worker's builds have no network policy yet.
export E2E_CHECK_BUILD_FIREWALL=0

exec "$SCRIPT_DIR/test-e2e.sh" "${1:-hello}" "${2:-8090}" "${3:-900}"
