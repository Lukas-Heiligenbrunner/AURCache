#!/usr/bin/env bash
# Build the binaries of a Debian image for the target architecture.
#
# server (docker/server.Dockerfile):
#   aurcache             - the server itself.
#   aurcache-sandbox     - confines `alpm-pkgbuild-bridge`, which parses a
#                          PKGBUILD by sourcing it, i.e. by executing it in the
#                          server's container. See backend/aurcache-sandbox.
# kube-worker (docker/kube-worker.Dockerfile):
#   aurcache-worker-kube - the dispatcher, linked dynamically like the server.
#   aurcache-build-agent - the agent each build pod runs, linked statically:
#                          the pod's `stage` init container copies it into the
#                          builder image, whose libc is not this image's.
#
# They are moved to predictable paths under target/ for the Dockerfile to copy.
#
# The argument is buildx's $TARGETPLATFORM. It is always cross-compiled from an
# amd64 builder stage, so the platform decides a rust target rather than being
# the host we happen to be on -- amd64 included, which only needs no toolchain.
# An unrecognised platform is an error, not a default: falling through to a
# native build is how an image ends up shipping an x86_64 binary under an arm64
# manifest, which builds clean and only fails at `exec format error` on the
# user's machine.
#
# Usage: build-rust.sh <platform> server|kube-worker   (common.sh beside it)
set -o pipefail
set -o errexit
set -o nounset
set -o verbose
set -o xtrace

PLATFORM="${1:?usage: build-rust.sh <platform> server|kube-worker}"
IMAGE="${2:?usage: build-rust.sh <platform> server|kube-worker}"

# The Arch name for the platform, and Debian's for its cross libc.
DEB_ARCH=
case "$PLATFORM" in
    linux/amd64) CARCH=x86_64 ;;
    linux/arm64) CARCH=aarch64 DEB_ARCH=arm64 ;;
    linux/arm/v7) CARCH=armv7h DEB_ARCH=armhf ;;
    *)
        echo "build-rust.sh: unsupported platform '$PLATFORM'" >&2
        exit 1
        ;;
esac
export CARCH

# The triples come from the helpers the PKGBUILDs build with, so the two
# builds cannot disagree about them.
# shellcheck source=packaging/common.sh
. "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"
RUST_TARGET=$(_aurcache_rust_target)

rustup target add "$RUST_TARGET"
if [ -n "$DEB_ARCH" ]; then
    prefix=$(_aurcache_cross_prefix)
    apt-get update -y
    # The libc for the target, named explicitly rather than left to apt's
    # Recommends: its headers, without which the C that `libsqlite3-sys` and
    # `aws-lc-sys` compile fails on `bits/libc-header-start.h: No such file or
    # directory` (a gcc error that says nothing about a missing package), and
    # its static archives for the agent.
    apt-get install -y --no-install-recommends "gcc-$prefix" "libc6-dev-$DEB_ARCH-cross"
    linker_var=${RUST_TARGET^^}
    export "CARGO_TARGET_${linker_var//-/_}_LINKER=$prefix-gcc"
fi

case "$IMAGE" in
    server)
        # `--features static` applies to aurcache only; the sandbox has no
        # such feature, so the two are built separately.
        cargo build --release --target="$RUST_TARGET" --features static
        cargo build --release --target="$RUST_TARGET" -p aurcache-sandbox
        mv "target/$RUST_TARGET/release/aurcache" target/aurcache
        mv "target/$RUST_TARGET/release/aurcache-sandbox" target/aurcache-sandbox
        ;;
    kube-worker)
        cargo build --release --target="$RUST_TARGET" -p aurcache-worker-kube
        # `+crt-static` reaches the agent and its dependencies but not build
        # scripts or proc macros, which cannot be static, because `--target`
        # is given. In a target directory of its own, so the differing flags
        # do not rebuild the dispatcher's dependencies.
        CARGO_TARGET_DIR=target/agent RUSTFLAGS="-C target-feature=+crt-static" \
            cargo build --release --target="$RUST_TARGET" -p aurcache-build-agent
        mv "target/$RUST_TARGET/release/aurcache-worker-kube" target/aurcache-worker-kube
        mv "target/agent/$RUST_TARGET/release/aurcache-build-agent" target/aurcache-build-agent
        # A dynamically linked agent would start in this image and fail in
        # the builder's: refuse it now. A dynamic executable has a program
        # interpreter, and a static one does not.
        if readelf --program-headers target/aurcache-build-agent | grep -q 'INTERP'; then
            echo "build-rust.sh: the agent is not statically linked" >&2
            exit 1
        fi
        ;;
    *)
        echo "build-rust.sh: unknown image '$IMAGE'" >&2
        exit 1
        ;;
esac
