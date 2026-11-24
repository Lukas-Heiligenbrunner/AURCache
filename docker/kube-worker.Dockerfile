# syntax=docker/dockerfile:1
#
# The Kubernetes worker (`aurcache-worker-kube`): the dispatcher, which builds
# each job in a pod of its own, and the build agent that those pods run.
#
# One image serves both. Its default entrypoint is the dispatcher. In a build
# pod, the `stage` init container runs `/usr/local/bin/aurcache-build-agent`
# from it to copy the agent into the pod, and the `hold` container runs
# `sleep infinity` from it, keeping the pod up while packages are read out
# through its agent. So it is published for every architecture that builds
# run on.
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
# which build-rust.sh takes the target triples from.
ARG TARGETPLATFORM
COPY docker/build-rust.sh packaging/common.sh /root/
RUN --mount=type=cache,target=/opt/cargo-cache,id=cargo-downloads-root \
    bash /root/build-rust.sh "$TARGETPLATFORM" kube-worker

# No `--platform` here: buildx already builds this stage for the target.
FROM debian:bookworm-slim
# ca-certificates for HTTPS to the server and the cluster's API, and
# openssh-client for the ssh-agent of a build that is given the SSH key.
# uid and gid 65532: the usual unprivileged pair.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 65532 aurcache \
    && useradd --uid 65532 --gid 65532 --no-create-home \
        --home-dir /var/lib/aurcache-worker --shell /usr/sbin/nologin aurcache \
    && install -d -o 65532 -g 65532 -m 0700 /var/lib/aurcache-worker
COPY --from=builder --chmod=0755 /app/backend/target/aurcache-worker-kube /usr/local/bin/aurcache-worker-kube
COPY --from=builder --chmod=0755 /app/backend/target/aurcache-build-agent /usr/local/bin/aurcache-build-agent

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/aurcache-worker-kube"]
