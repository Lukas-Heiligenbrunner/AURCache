#!/usr/bin/env bash
# Build the Kubernetes worker's binaries for the target architecture: the
# builder stage of docker/kube-worker.Dockerfile.
#
# Two binaries are produced:
#   aurcache-worker-kube - the dispatcher, linked dynamically against the
#                          image's glibc like the server.
#   aurcache-build-agent - the agent each build pod runs, linked statically:
#                          the pod's `stage` init container copies it into the
#                          builder image, whose libc is not this image's.
#
# Both are moved to predictable paths under target/ for the Dockerfile to copy.
#
# The argument is buildx's $TARGETPLATFORM. It is always cross-compiled from an
# amd64 builder stage, so the platform decides a rust target rather than being
# the host the build happens to run on -- amd64 included, which only needs no
# toolchain. An unrecognised platform is an error, not a default: falling
# through to a native build would ship an x86_64 binary under a foreign
# manifest, which builds clean and fails only at `exec format error`.
#
# Usage: build-kube-worker.sh <platform>   (common.sh beside it)
set -o pipefail
set -o errexit
set -o nounset
set -o xtrace

PLATFORM="${1:?usage: build-kube-worker.sh <platform>}"

# The Arch name for the platform, and Debian's for its cross libc.
DEB_ARCH=
case "$PLATFORM" in
    linux/amd64) CARCH=x86_64 ;;
    linux/arm64) CARCH=aarch64 DEB_ARCH=arm64 ;;
    linux/arm/v7) CARCH=armv7h DEB_ARCH=armhf ;;
    *)
        echo "build-kube-worker.sh: unsupported platform '$PLATFORM'" >&2
        exit 1
        ;;
esac
export CARCH

# The triples come from the helpers the PKGBUILDs build with, so the image and
# the packages cannot disagree about them.
# shellcheck source=packaging/common.sh
. "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"
RUST_TARGET=$(_aurcache_rust_target)

rustup target add "$RUST_TARGET"
if [ -n "$DEB_ARCH" ]; then
    prefix=$(_aurcache_cross_prefix)
    apt-get update -y
    # The cross compiler, and the target's libc named explicitly rather than
    # left to apt's Recommends: its headers for the C that aws-lc-sys compiles,
    # and its static archives (libc.a) for the agent.
    apt-get install -y --no-install-recommends "gcc-$prefix" "libc6-dev-$DEB_ARCH-cross"
    linker_var=${RUST_TARGET^^}
    export "CARGO_TARGET_${linker_var//-/_}_LINKER=$prefix-gcc"
fi

cargo build --release --target="$RUST_TARGET" -p aurcache-worker-kube

# Always with `--target`, so `+crt-static` reaches the agent and its
# dependencies but not build scripts or proc macros, which cannot be static.
# In a target directory of its own, so the differing flags do not rebuild the
# dispatcher's dependencies.
CARGO_TARGET_DIR=target/agent RUSTFLAGS="-C target-feature=+crt-static" \
    cargo build --release --target="$RUST_TARGET" -p aurcache-build-agent

mv "target/$RUST_TARGET/release/aurcache-worker-kube" target/aurcache-worker-kube
mv "target/agent/$RUST_TARGET/release/aurcache-build-agent" target/aurcache-build-agent

# A dynamically linked agent would start in this image and fail in the
# builder's, far from here: refuse it now. The program interpreter is what a
# dynamic executable has and a static one does not.
if readelf --program-headers target/aurcache-build-agent | grep -q 'INTERP'; then
    echo "build-kube-worker.sh: the agent is not statically linked" >&2
    exit 1
fi
