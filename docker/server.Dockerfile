# syntax=docker/dockerfile:1
# What the server binary reports as its version (see aurcache-common's build
# script): the commit, except on release builds, which name the tag instead.
ARG LATEST_COMMIT_SHA=dev
ARG AURCACHE_GIT_TAG=
ARG AURCACHE_GIT_DIRTY=
# Must stay on the same Debian release as the runtime stage below: the default
# `rust:1.98.1` tag is trixie (glibc 2.41) and produces a binary the bookworm
# runtime (glibc 2.36) cannot load - it dies at startup with
# "GLIBC_2.38 not found".
FROM --platform=linux/amd64 rust:1.98.1-bookworm AS builder
ARG LATEST_COMMIT_SHA
ARG AURCACHE_GIT_TAG
ARG AURCACHE_GIT_DIRTY
ENV LATEST_COMMIT_SHA=${LATEST_COMMIT_SHA} \
    AURCACHE_GIT_TAG=${AURCACHE_GIT_TAG} \
    AURCACHE_GIT_DIRTY=${AURCACHE_GIT_DIRTY}
# Install necessary tools and dependencies

# The official rust image keeps cargo and the toolchain under /usr/local/cargo,
# which a cache mount would *hide* on the first, cold build -- so the cache home
# lives here instead, and only the crates.io-visible bits of $CARGO_HOME (the
# registry, and command binaries for `cargo install`) are redirected to it. The
# `cargo` binary itself still resolves from /usr/local/cargo/bin on the layer.
#
# The CLI's version has to match the `wasm-bindgen` crate the frontend links
# against exactly, so it is read from the frontend's lockfile -- see
# install-wasm-bindgen.sh for the whole rationale, shared with the hybrid image.
#
# The lockfile alone, ahead of the source, so that this expensive `cargo
# install` is invalidated when the pinned version changes and not by a code
# edit -- the same ordering, and for the same reason, as hybrid.Dockerfile.
COPY frontend-rs/Cargo.lock /tmp/frontend-Cargo.lock
ADD --chmod=0755 docker/install-wasm-bindgen.sh /root/
# The cargo cache is mounted: crates and the installed wasm-bindgen-cli are
# immutable per version, so they are reused between builds and across the three
# target architectures instead of being fetched once each. The `-root` cache id
# stays private to this Debian builder stage -- the Arch packager stages keep
# their own (`-packager`), because a compiled tool like wasm-bindgen-cli that
# landed in the shared `bin/` from the other family would be linked against the
# other's libc and could refuse to load. The downloads themselves are
# toolchain-independent; the separation is about the binaries that share this
# home, not the crates. (rustup stays on the shipped layer, as today: its std
# downloads are already layer-cached per architecture.)
ENV CARGO_HOME=/opt/cargo-cache \
    PATH="/opt/cargo-cache/bin:${PATH}"
# `cargo install` puts the binary in that cache mount, and a cache mount is
# buildkit scratch space: `docker builder prune` or buildkit GC can evict it
# without touching the layer cache, leaving a cached install step with no
# binary behind it. Copy it into the image layer so the `cargo build` below
# does not depend on what the cache store happens to still hold. `/usr/local/bin`
# is on the image's default PATH, which is where build-rust.sh's nested cargo
# looks for it.
RUN --mount=type=cache,target=/opt/cargo-cache,id=cargo-downloads-root \
    bash /root/install-wasm-bindgen.sh /tmp/frontend-Cargo.lock \
    && install -Dm755 /opt/cargo-cache/bin/wasm-bindgen /usr/local/bin/wasm-bindgen

# The two trees keep the layout they have in the repository: the frontend
# reaches the shared crates as `../backend/...`, and `aurcache-api`'s build
# script reaches the frontend as `../../frontend-rs`. Flattening either breaks
# the other.
#
# The frontend is compiled by that build script, which the `static` feature in
# build-rust.sh switches on. There is no separate frontend stage, because one
# would have to be kept in step with the build script by hand — which is the
# coupling the script exists to remove.
COPY backend/ /app/backend/
COPY frontend-rs /app/frontend-rs/
# The logo the build script embeds beside the frontend, from the same relative
# place (`../../assets`).
COPY assets/icons/icon.svg /app/assets/icons/icon.svg
WORKDIR /app/backend

# Which architecture to cross-compile for. `TARGETPLATFORM` is what buildx sets
# per target in a multi-platform build, so it is the default; `TARGET_ARCH`
# stays as an explicit override for the publish workflow, which builds one
# architecture at a time and names it itself. build-rust.sh accepts either
# spelling and rejects anything it does not recognise.
ARG TARGETPLATFORM
ARG TARGET_ARCH=
ADD docker/build-rust.sh /root
RUN --mount=type=cache,target=/opt/cargo-cache,id=cargo-downloads-root \
    bash /root/build-rust.sh "${TARGET_ARCH:-$TARGETPLATFORM}"

# No `--platform` here: buildx already builds this stage for the target, and
# pinning it to a build arg meant the runtime base and the binary above could
# be selected by two different values and disagree.
FROM debian:bookworm-slim
# Podman/Docker is no longer used at runtime (remote workers build packages),
# so the server just needs a minimal glibc base. This must stay on the same
# Debian release as the builder stage above (see the note there). Runtime deps:
# ca-certificates (HTTPS to the AUR), bash (the alpm-pkgbuild-bridge PKGBUILD
# parser is a bash script) and tzdata (so `TZ=Europe/Paris` resolves).
#
# The image's own `/etc/localtime` link to Etc/UTC goes: the compose files
# bind-mount the host's over it, which would otherwise land on the link's
# target and leave the server naming the host's zone "Etc/UTC". Without either
# it is UTC all the same.
#
# `aurcache-parse` is who a PKGBUILD parse runs as on a kernel without
# Landlock (see backend/aurcache-sandbox/src/parse.rs); private-state.sh closes
# the server's directories to it at every start.
#
# Its uid is fixed, and away from the system range: left to `useradd
# --system` it got 999, which is also the postgres image's uid -- the owner
# of a database directory a deployment keeps under the server's own data,
# which the parse must not reach (and the sandbox then rightly refused to
# parse at all).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates bash tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && rm -f /etc/localtime /etc/timezone \
    && groupadd --gid 58400 aurcache-parse \
    && useradd --system --uid 58400 --gid 58400 --no-create-home \
        --home-dir /nonexistent --shell /usr/sbin/nologin aurcache-parse
# Copy the built binary from the previous stage
COPY --from=builder --chmod=0755 /app/backend/target/aurcache /usr/local/bin/aurcache
COPY --chmod=0755 docker/entrypoint.sh /entrypoint.sh
COPY --chmod=0755 docker/private-state.sh /usr/local/bin/aurcache-private-state

# alpm-pkgbuild-bridge parses a PKGBUILD by sourcing it, i.e. by executing
# attacker-supplied bash in the server process's container. The server runs it
# through aurcache-sandbox, both by absolute path -- see
# backend/aurcache-utils/src/pkgbuild.rs and backend/aurcache-sandbox. It stays
# out of PATH so nothing reaches it by name.
ADD --chmod=755 https://gitlab.archlinux.org/archlinux/alpm/alpm-pkgbuild-bridge/-/raw/main/alpm-pkgbuild-bridge.sh?ref_type=heads /usr/local/libexec/alpm-pkgbuild-bridge
COPY --from=builder --chmod=0755 /app/backend/target/aurcache-sandbox /usr/local/bin/aurcache-sandbox
# The defaults are the native packages' paths; this image keeps both under
# /usr/local, so point the server at them.
ENV AURCACHE_PKGBUILD_BRIDGE=/usr/local/libexec/alpm-pkgbuild-bridge \
    AURCACHE_SANDBOX=/usr/local/bin/aurcache-sandbox

WORKDIR /app
CMD ["/entrypoint.sh"]
