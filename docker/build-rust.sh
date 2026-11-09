#!/usr/bin/env bash
# Build the server binaries for the target architecture.
#
# Two binaries are produced:
#   aurcache         - the server itself.
#   aurcache-sandbox - confines `alpm-pkgbuild-bridge`, which parses a PKGBUILD
#                      by sourcing it, i.e. by executing it in the server's
#                      container. See backend/aurcache-sandbox.
#
# Both are moved to predictable paths under target/ for the Dockerfile to copy.
#
# The argument is buildx's $TARGETPLATFORM. It is always cross-compiled from an
# amd64 builder stage, so the platform decides a rust target rather than being
# the host we happen to be on -- amd64 included, which only needs no toolchain.
# An unrecognised platform is an error, not a default: falling through to a
# native build is how an image ends up shipping an x86_64 binary under an arm64
# manifest, which builds clean and only fails at `exec format error` on the
# user's machine.
#
# Usage: build-rust.sh <platform>   (common.sh beside it)
set -o pipefail
set -o errexit
set -o nounset
set -o verbose
set -o xtrace

PLATFORM="${1:?usage: build-rust.sh <platform>}"

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
    # The libc headers for the target, named explicitly rather than left to
    # apt's Recommends. `libsqlite3-sys` compiles bundled SQLite C sources with
    # the cross compiler, and without the target's headers that fails on
    # `bits/libc-header-start.h: No such file or directory` -- a gcc error that
    # says nothing about a missing Debian package.
    apt-get install -y --no-install-recommends "gcc-$prefix" "libc6-dev-$DEB_ARCH-cross"
    linker_var=${RUST_TARGET^^}
    export "CARGO_TARGET_${linker_var//-/_}_LINKER=$prefix-gcc"
fi

# `--features static` applies to aurcache only; the sandbox has no such feature,
# so the two are built separately rather than in one invocation.
cargo build --release --target="$RUST_TARGET" --features static
cargo build --release --target="$RUST_TARGET" -p aurcache-sandbox
mv "target/$RUST_TARGET/release/aurcache" target/aurcache
mv "target/$RUST_TARGET/release/aurcache-sandbox" target/aurcache-sandbox
