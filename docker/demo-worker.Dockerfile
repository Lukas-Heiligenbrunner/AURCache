# syntax=docker/dockerfile:1
# What the demo worker binary reports as its version (see aurcache-common's
# build script): the commit, except on release builds, which name the tag
# instead.
ARG LATEST_COMMIT_SHA=dev
ARG AURCACHE_GIT_TAG=
ARG AURCACHE_GIT_DIRTY=
# Must stay on the same Debian release as the runtime stage below: the default
# `rust:1.98.1` tag is trixie (glibc 2.41) and produces a binary the bookworm
# runtime (glibc 2.36) cannot load - it dies at startup with
# "GLIBC_2.38 not found". Same pin as docker/server.Dockerfile.
#
# No `--platform` here: this stage builds natively for each target platform
# buildx is asked for (emulated where the builder cannot run it), so the
# binary always matches the runtime base below. Pinning it to $BUILDPLATFORM
# would ship a builder-arch binary under a foreign manifest.
FROM rust:1.98.1-bookworm AS builder
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
# Same arrangement as docker/server.Dockerfile.
ENV CARGO_HOME=/opt/cargo-cache \
    PATH="/opt/cargo-cache/bin:${PATH}"

# Only the backend tree: the demo worker links aurcache-common and
# aurcache-worker-core, neither of which reaches the frontend, so unlike the
# server image this needs no frontend sources and no wasm toolchain.
COPY backend/ /app/backend/
WORKDIR /app/backend

# Built natively for whichever platform buildx targets -- deliberately no
# cross-compilation. The demo worker has no chroot, no devtools and no
# arch-specific runtime needs (plain protocol client plus zstd), so a native
# per-platform build is correct everywhere; under qemu emulation it is slow
# rather than wrong, which is worth knowing before wondering why an arm64
# build takes a while on an amd64 host.
ARG TARGETPLATFORM
RUN --mount=type=cache,target=/opt/cargo-cache,id=cargo-downloads-root \
    cargo build --release -p aurcache-worker-demo

# No `--platform` here: buildx already builds this stage for the target, and
# pinning it to a build arg meant the runtime base and the binary above could
# be selected by two different values and disagree.
FROM debian:bookworm-slim
# Runtime deps: ca-certificates for the worker's HTTPS/mTLS exchanges with the
# server. This must stay on the same Debian release as the builder stage above
# (see the note there).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder --chmod=0755 /app/backend/target/release/aurcache-worker-demo /usr/local/bin/aurcache-worker-demo

# The worker's persisted mTLS identity. Kept on a volume in compose so a
# recreated container does not re-enroll as a new, unapproved machine.
ENV WORKER_DATA_DIR=/var/lib/aurcache-worker
VOLUME ["/var/lib/aurcache-worker"]

WORKDIR /var/lib/aurcache-worker
# Default: enroll and poll for jobs. Unlike the real worker this needs no
# entrypoint wrapper (no chroot, no cgroups, nothing privileged) and no
# command arguments.
CMD ["/usr/local/bin/aurcache-worker-demo"]
