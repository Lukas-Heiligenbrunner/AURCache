#!/usr/bin/env bash
# End-to-end test for the hybrid compatibility image.
#
# Drives the shared harness (scripts/test-e2e.sh) against a compose file shaped
# like the pre-worker single-container setup, to check that an existing
# deployment keeps building after upgrading to the new `aurcache` image without
# editing anything.
#
# What this proves that the ordinary e2e does not:
#   * the server and an embedded worker start together in one container;
#   * the worker enrolls over loopback using the shared secret the entrypoint
#     generates, with no enrollment volume and no exposed mTLS port;
#   * with no Docker socket mounted, the entrypoint starts Podman inside the
#     container, as the old DinD image did, and the legacy builder drives it;
#   * a package builds and lands in the repository.
#
# Usage: scripts/test-e2e-hybrid.sh [package] [port] [timeout]
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

E2E_COMPOSE_FILE="$(dirname "$SCRIPT_DIR")/compose/docker-compose.e2e-hybrid.yaml"
export E2E_COMPOSE_FILE
# One container fills both roles, so only one service must stay alive.
export E2E_SERVICES="aurcache"
# The container builder has no storage pool, so its builds report no disk use.
export E2E_EXPECT_DISK_USAGE=0

exec "$SCRIPT_DIR/test-e2e.sh" "${1:-hello}" "${2:-8090}" "${3:-900}"
