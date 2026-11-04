#!/usr/bin/env bash
# Cross-compile AURCache packages for the build's target architecture into
# /pkg: the packager stage of docker/arch.Dockerfile, which builds every
# package its images install. Which packages is the caller's argument.
#
# Arch has no cross-compilation feature -- no `--target`, nothing in devtools --
# but `CARCH` is a plain shell variable makepkg sources and uses for the package
# name, so setting it and cross-compiling inside build() produces a correctly
# labelled package from one PKGBUILD. That is a convention, not a supported
# mode, which is why it is written down here rather than assumed.
#
# Every architecture runs this at once over the same rustup home, so rustup is
# only ever reached through install-rust-toolchain.sh, which serializes it and
# restores a toolchain the cache mount lost. The compiles themselves share the
# cargo home freely: cargo locks its own caches.
set -euxo pipefail

case "${TARGETARCH}${TARGETVARIANT:-}" in
    amd64) CARCH=x86_64 ;;
    arm64) CARCH=aarch64 ;;
    armv7) CARCH=armv7h ;;
    *)
        echo "unsupported TARGETARCH=${TARGETARCH}${TARGETVARIANT:-}" >&2
        exit 1
        ;;
esac
export CARCH

# The Rust target triple comes from common.sh, which the PKGBUILDs source
# too, so this cannot drift from the one cargo is asked for.
. /src/packaging/common.sh
targets=("$(_aurcache_rust_target)")
# The server embeds its frontend, which aurcache-api's build script compiles to
# wasm. Its JS glue comes from a build dependency, so the std is all it needs.
if [[ " $* " == *" aurcache-server "* ]]; then
    targets+=(wasm32-unknown-unknown)
fi
install-rust-toolchain.sh "${targets[@]}"

for package in "$@"; do
    /src/packaging/build-from-tree.sh /src "$package" /pkg
done
