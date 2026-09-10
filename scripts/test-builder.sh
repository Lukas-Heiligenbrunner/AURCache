#!/usr/bin/env bash
# Run the test-builder binary from the repo root.
#
# Usage: scripts/test-builder.sh [<package>] [<builder-image>] [<build-flags>]
#
#   <package>        AUR package to build              (default: hello)
#   <builder-image>  Docker image tag to build/use     (default: aurcache-builder:test)
#   <build-flags>    Extra makepkg flags                (default: --noconfirm --noprogressbar --nocolor)
#
# Must be run from the repository root (docker/builder.Dockerfile is referenced relative to cwd).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PACKAGE="${1:-hello}"
BUILDER_IMAGE="${2:-aurcache-builder:test}"
BUILD_FLAGS="${3:---noconfirm --noprogressbar --nocolor}"

cargo run \
  --manifest-path backend/Cargo.toml \
  --package aurcache-builder \
  --bin test-builder \
  -- \
  "$PACKAGE" \
  "$BUILDER_IMAGE" \
  "$BUILD_FLAGS"
