#!/usr/bin/env bash
# End-to-end test for the hybrid image's legacy container builder.
#
# Covers the upgrade path of a pre-worker deployment that used "host build
# mode": Docker socket mounted, BUILD_ARTIFACT_DIR set, container not
# privileged. That combination selects the legacy builder, which spawns a
# container per package instead of building in a chroot.
#
# Usage: scripts/test-e2e-hybrid-legacy.sh [package] [port] [timeout]
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# The builder image the legacy executor spawns. Built locally so the test does
# not depend on a published tag.
docker build -q -f "$PROJECT_DIR/docker/builder.Dockerfile" \
    -t aurcache-legacy-builder:test "$PROJECT_DIR" >/dev/null

# Shared with the Docker daemon by absolute path, so it must exist on the host.
mkdir -p /tmp/aurcache-legacy-builds

export E2E_COMPOSE_FILE="$PROJECT_DIR/compose/docker-compose.e2e-hybrid-legacy.yaml"
export E2E_SERVICES="aurcache"
# The container builder has no storage pool, so its builds report no disk use.
export E2E_EXPECT_DISK_USAGE=0

exec "$SCRIPT_DIR/test-e2e.sh" "${1:-hello}" "${2:-8090}" "${3:-900}"
