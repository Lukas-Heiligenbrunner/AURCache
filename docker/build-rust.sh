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
# The argument is a platform string. It is always cross-compiled from an amd64
# builder stage, so the platform decides a rust target rather than being the
# host we happen to be on.
set -o pipefail
set -o errexit
set -o nounset
set -o verbose
set -o xtrace

TARGET_ARCH="${1:?usage: build-rust.sh <platform>}"

# Both spellings of every platform are accepted deliberately. buildx reports
# `linux/arm64` through TARGETPLATFORM where the publish workflow passes
# `linux/arm64/v8`, and the two mean the same target. Matching one spelling and
# quietly falling through to a native build for everything else is how an image
# ends up shipping an x86_64 binary under an arm64 manifest -- which builds
# clean and only fails at `exec format error` on the user's machine. An
# unrecognised platform is therefore an error, not a default.
case "$TARGET_ARCH" in
    linux/amd64 | linux/x86_64 | amd64 | x86_64)
        RUST_TARGET=
        ;;
    linux/arm64 | linux/arm64/* | arm64 | aarch64)
        RUST_TARGET=aarch64-unknown-linux-gnu
        GCC_PACKAGE=gcc-aarch64-linux-gnu
        LIBC_PACKAGE=libc6-dev-arm64-cross
        LINKER=aarch64-linux-gnu-gcc
        LINKER_VAR=CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER
        ;;
    linux/arm/v7 | linux/armv7* | arm/v7 | armv7*)
        RUST_TARGET=armv7-unknown-linux-gnueabihf
        GCC_PACKAGE=gcc-arm-linux-gnueabihf
        LIBC_PACKAGE=libc6-dev-armhf-cross
        LINKER=arm-linux-gnueabihf-gcc
        LINKER_VAR=CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER
        ;;
    *)
        echo "build-rust.sh: unsupported platform '$TARGET_ARCH'" >&2
        exit 1
        ;;
esac

# `--features static` applies to aurcache only; the sandbox has no such feature,
# so the two are built separately rather than in one invocation.
if [ -n "$RUST_TARGET" ]; then
    rustup target add "$RUST_TARGET"
    apt-get update -y
    # The libc headers for the target, named explicitly rather than left to
    # apt's Recommends. `libsqlite3-sys` compiles bundled SQLite C sources with
    # the cross compiler, and without the target's headers that fails on
    # `bits/libc-header-start.h: No such file or directory` -- a gcc error that
    # says nothing about a missing Debian package.
    apt-get install -y --no-install-recommends "$GCC_PACKAGE" "$LIBC_PACKAGE"
    export "$LINKER_VAR=$LINKER"
    cargo build --release --target="$RUST_TARGET" --features static
    cargo build --release --target="$RUST_TARGET" -p aurcache-sandbox
    mv "target/$RUST_TARGET/release/aurcache" target/aurcache
    mv "target/$RUST_TARGET/release/aurcache-sandbox" target/aurcache-sandbox
else
    cargo build --release --features static
    cargo build --release -p aurcache-sandbox
    mv target/release/aurcache target/aurcache
    mv target/release/aurcache-sandbox target/aurcache-sandbox
fi
