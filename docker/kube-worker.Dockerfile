# syntax=docker/dockerfile:1
#
# The Kubernetes worker (`aurcache-worker-kube`): the dispatcher, which builds
# each job in a pod of its own, and the build agent those pods run.
#
# One image serves both. Its default entrypoint is the dispatcher; the build
# pods start it again with `/usr/local/bin/aurcache-build-agent` as their
# command: the `stage` init container copies the agent into the pod, the
# `hold` container keeps the pod up while packages are read out, and the
# network probe's pod runs `probe`. So it exists for every architecture builds
# run on, and runs on every node that builds.
#
# What the binaries report as their version (see aurcache-common's build
# script): the commit, except on release builds, which name the tag instead.
ARG LATEST_COMMIT_SHA=dev
ARG AURCACHE_GIT_TAG=
ARG AURCACHE_GIT_DIRTY=
# Must stay on the same Debian release as the runtime stage below: the default
# `rust` tag is trixie (glibc 2.41) and produces a binary the bookworm runtime
# (glibc 2.36) cannot load. Same pin as docker/server.Dockerfile.
#
# Pinned to amd64 and cross-compiled, as the server is: cargo under qemu would
# turn minutes into an hour.
FROM --platform=linux/amd64 rust:1.100.0-bookworm AS builder
ARG LATEST_COMMIT_SHA
ARG AURCACHE_GIT_TAG
ARG AURCACHE_GIT_DIRTY
ENV LATEST_COMMIT_SHA=${LATEST_COMMIT_SHA} \
    AURCACHE_GIT_TAG=${AURCACHE_GIT_TAG} \
    AURCACHE_GIT_DIRTY=${AURCACHE_GIT_DIRTY}

# The registry and command binaries in a cache mount of their own, and the
# toolchain left on the image's layer, as in docker/server.Dockerfile, where
# the arrangement is explained. The cache id is that image's: both are Debian
# builder stages, so the crates they download are the same.
ENV CARGO_HOME=/opt/cargo-cache \
    PATH="/opt/cargo-cache/bin:${PATH}"

# Only the backend tree: neither binary reaches the frontend.
COPY backend/ /app/backend/
WORKDIR /app/backend

# Which architecture to cross-compile for: buildx sets `TARGETPLATFORM` per
# target in a multi-platform build. common.sh is the PKGBUILDs' own helpers,
# which build-kube-worker.sh takes the target triples from.
ARG TARGETPLATFORM
COPY docker/build-kube-worker.sh packaging/common.sh /root/
RUN --mount=type=cache,target=/opt/cargo-cache,id=cargo-downloads-root \
    bash /root/build-kube-worker.sh "$TARGETPLATFORM"

# No `--platform` here: buildx already builds this stage for the target.
FROM debian:bookworm-slim
# ca-certificates for the dispatcher's HTTPS to the server and the cluster's
# API. Nothing else: it runs no pacman, no git and no shell script.
#
# uid and gid 65532, the usual unprivileged pair for a container that needs no
# account of its own; the shipped Deployment runs it as that, with that
# fsGroup, a read-only root filesystem and an emptyDir at /tmp. The dispatcher
# writes nowhere but /tmp and its data directory.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 65532 aurcache \
    && useradd --uid 65532 --gid 65532 --no-create-home \
        --home-dir /var/lib/aurcache-worker --shell /usr/sbin/nologin aurcache \
    && install -d -o 65532 -g 65532 -m 0700 /var/lib/aurcache-worker
COPY --from=builder --chmod=0755 /app/backend/target/aurcache-worker-kube /usr/local/bin/aurcache-worker-kube
COPY --from=builder --chmod=0755 /app/backend/target/aurcache-build-agent /usr/local/bin/aurcache-build-agent

# The worker's persisted mTLS identity, on a volume so a recreated container
# does not enroll again as a new, unapproved machine.
ENV WORKER_DATA_DIR=/var/lib/aurcache-worker
VOLUME ["/var/lib/aurcache-worker"]

USER 65532:65532
WORKDIR /var/lib/aurcache-worker
ENTRYPOINT ["/usr/local/bin/aurcache-worker-kube"]
