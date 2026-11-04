# syntax=docker/dockerfile:1
#
# The Arch-based images, multi-arch (amd64, arm64 and armv7). Select one with
# `--target`; scripts/build-images.sh does.
#
#   worker  the remote build worker (`aurcache-worker`). It runs one long-lived
#           process per architecture and builds every package in a `devtools`
#           chroot. Foreign architectures run this same image emulated via
#           qemu-user + binfmt (see docker-compose.yaml), so the build path is
#           uniform across arches -- an aarch64 worker is just this image run
#           with `--platform linux/arm64`. Run privileged: arch-nspawn /
#           mkarchroot need mount + unshare.
#
#   hybrid  the compatibility image (`aurcache`): server + an embedded build
#           worker. **Deprecated.** It exists so that deployments predating the
#           remote-worker architecture keep working after an upgrade without
#           editing their compose file. New deployments should run the split
#           images: `aurcache-server` plus one or more `aurcache-worker`
#           containers, which can be scaled and placed independently. See the
#           Build Workers documentation.
#
#           The embedded worker is the legacy container builder: the one
#           closest to what these deployments ran before, so their
#           requirements stay what they were. In the old DinD mode
#           (privileged, no socket mounted) it drives a Podman running in this
#           container, as the old image did; in host mode (`BUILD_ARTIFACT_DIR`
#           set, the host's socket mounted) it drives the host's daemon.
#
# One packager stage cross-compiles every package both images install, on the
# native build host (fast -- no cargo emulation). The runtime stages install
# those packages into a minimal Arch base for the target arch, so an image and
# a native install are the same thing.
#
# armv7's cross toolchain is not in Arch's official repositories, so it is
# built from the AUR in a stage of its own -- see the toolchain stages below
# for why that costs nothing on the other two.

# What the binaries report as their version: the commit they were built from
# (see aurcache-common's build script). The tag stays empty except for release
# builds, so ordinary images name their commit instead.
ARG LATEST_COMMIT_SHA=dev
ARG AURCACHE_GIT_TAG=
ARG AURCACHE_GIT_DIRTY=

########## Stage 1: cross-compile and package ##########
# Runs natively on the builder and cross-compiles, which is the whole reason
# this is not simply built in the target-arch image: cargo under qemu turns
# minutes into a great deal longer.
#
# Arch has no cross-compilation feature, but `CARCH` is a plain shell variable
# makepkg uses for the package name, and aarch64's toolchain is in `extra`. So
# exporting CARCH and cross-compiling inside build() yields a correctly labelled
# package from one PKGBUILD -- the same one a native install uses.
#
# Pinned to amd64 rather than $BUILDPLATFORM: the official Arch image is
# x86_64-only (ARM Linux uses the lopsided/ images below), so there is no native
# packager base on an aarch64 builder -- and Arch Linux ARM packages no x86_64
# cross toolchain either, so an ARM host could not produce the amd64 package
# even if there were. Building the images therefore needs an amd64 host; on
# anything else this stage runs emulated and slowly rather than failing, which
# is worth knowing before wondering why a build takes an hour.
FROM --platform=linux/amd64 archlinux/archlinux:latest AS packager-base
# The cache id carries the platform: a buildkit cache mount is keyed on its
# target path, so without this the amd64 packager's x86_64 downloads land in
# the very cache the arm64 and armv7 runtimes read from. That surfaces as
# "could not find package in cache" and as signatures from the Arch Linux ARM
# build system being rejected -- neither of which points at a shared cache.
RUN --mount=type=cache,target=/var/cache/pacman/pkg,id=pacman-packager \
    sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf \
    # rustup rather than the `rust` package: Arch ships std for the host
    # architecture only, so `cargo build --target aarch64-...` fails with
    # "can't find crate for `std`". rustup fetches precompiled std per target,
    # wasm32 included.
    && pacman -Syu --noconfirm --needed base-devel rustup git \
        aarch64-linux-gnu-gcc \
    # makepkg refuses to run as root, and rightly: a PKGBUILD is a shell script.
    && useradd --create-home packager \
    # The armv7 toolchain stage installs each link of the chain as it builds
    # it, so the unprivileged build user needs pacman without a password.
    # Nothing else here installs anything -- makepkg runs --nodeps.
    && echo 'packager ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/packager \
    # The build stage runs unprivileged, so its output directory has to exist
    # and be writable before the drop to `packager`.
    && install -d -o packager /pkg

########## Stage 1b: the cross toolchain for the target ##########
# amd64 and arm64 need nothing beyond what the base already has: the build is
# native for one and `aarch64-linux-gnu-gcc` from `extra` covers the other.
FROM packager-base AS toolchain-amd64
FROM packager-base AS toolchain-arm64

# armv7h's cross toolchain is not in any official repository, so it is built
# from the AUR -- seven packages, gcc three times, the better part of an hour.
#
# Deliberately *before* any source is copied in. Docker caches this layer and
# only rebuilds it when the script itself changes, but only if no earlier layer
# has been invalidated: put it after `COPY . /src` and every code change pays
# the hour again. CI runners start cold, so the workflow feeds buildx a cache.
FROM packager-base AS toolchain-armv7
# Point this at a pacman repository holding prebuilt cross-toolchain packages
# and the bootstrap is skipped for whatever it carries -- an AURCache instance
# serves nicely. Anything the repository lacks is still built from the AUR, so
# an unset or unreachable value costs correctness nothing, only time.
ARG AURCACHE_TOOLCHAIN_REPO=
ENV AURCACHE_TOOLCHAIN_REPO=${AURCACHE_TOOLCHAIN_REPO}
# The repository's section name, which decides the database file pacman asks
# for. Defaults to what an AURCache instance serves.
ARG AURCACHE_TOOLCHAIN_REPO_NAME=repo
ENV AURCACHE_TOOLCHAIN_REPO_NAME=${AURCACHE_TOOLCHAIN_REPO_NAME}
COPY --chmod=0755 packaging/build-cross-toolchain.sh /usr/local/bin/build-cross-toolchain
USER packager
RUN build-cross-toolchain
USER root

ARG TARGETARCH
ARG TARGETVARIANT
FROM toolchain-${TARGETARCH}${TARGETVARIANT:+${TARGETVARIANT}} AS packager
ARG TARGETARCH
ARG TARGETVARIANT
ARG LATEST_COMMIT_SHA
ARG AURCACHE_GIT_TAG
ARG AURCACHE_GIT_DIRTY
ENV LATEST_COMMIT_SHA=${LATEST_COMMIT_SHA} \
    AURCACHE_GIT_TAG=${AURCACHE_GIT_TAG} \
    AURCACHE_GIT_DIRTY=${AURCACHE_GIT_DIRTY}

# The scripts need no source, so they are copied ahead of the tree and a code
# change does not invalidate them.
USER packager
COPY --chmod=0755 docker/install-rust-toolchain.sh docker/build-aurcache-packages.sh /usr/local/bin/

COPY --chown=packager . /src
# Every package either image installs, built once. aurcache-sandbox is its own
# package because the worker and the server both need the binary and pacman
# refuses two packages shipping one path. See build-aurcache-packages.sh for
# the build itself.
#
# Cargo and rustup home dirs are cache mounts, so `prepare()`'s `cargo fetch
# --locked` and the per-architecture `rustup target add` stop downloading after
# the first build. The mounts are shared between the architectures building at
# once; rustup is serialized inside them by install-rust-toolchain.sh, which
# build-aurcache-packages.sh calls first, and which also reinstalls the
# toolchain if buildkit evicted the mount. The `uid`/`gid` are the whole point:
# buildkit would otherwise create the mount as root and cargo could not write
# `~/.cargo/.crates.toml`.
RUN --mount=type=cache,target=/home/packager/.cargo,id=cargo-downloads-packager,uid=1000,gid=1000,mode=0700 \
    --mount=type=cache,target=/home/packager/.rustup,id=rustup-downloads-packager,uid=1000,gid=1000,mode=0700 \
    build-aurcache-packages.sh aurcache-sandbox aurcache-worker aurcache-server aurcache-worker-docker

########## Stage 1c: export the built packages to the host ##########
# The images install the packages straight from the packager stage, which no
# image ships, but a host that wants them for a native install should not
# re-make them with build-packages.sh. This stage gives the finished archives
# back: nothing in the image flow references it, so it costs the normal build
# nothing, and `scripts/build-images.sh --packages-dir` runs one extra
# `buildx build --target export-pkgs` that replays the now-cached (host-arch)
# packager stage and copies /pkg out with `type=local` output.
FROM scratch AS export-pkgs
COPY --from=packager /pkg/*.pkg.tar.zst /

########## Stage 2: per-arch Arch Linux runtime base ##########
# Official Arch is x86_64-only; Arch Linux ARM images cover arm64 and armv7.
FROM --platform=linux/amd64 archlinux/archlinux:latest AS runtime-amd64
FROM --platform=linux/arm64 lopsided/archlinux:latest AS runtime-arm64
FROM --platform=linux/arm/v7 lopsided/archlinux-arm32v7:latest AS runtime-armv7

# Select the base matching the target arch (buildkit resolves the alias).
ARG TARGETARCH
ARG TARGETVARIANT
FROM runtime-${TARGETARCH}${TARGETVARIANT:+${TARGETVARIANT}} AS runtime
# Interpolated into the pacman cache id below.
ARG TARGETPLATFORM

# DisableSandbox: pacman 7's Landlock-based download sandbox needs syscalls
# that some runtimes block or do not implement (older seccomp profiles, qemu
# emulation), which makes every `pacman -Sy` abort there. Disabling it keeps
# these images working everywhere; it only affects pacman's own download
# isolation, not the per-build isolation (each build still runs in its own
# chroot or container).
#
# The keyring's master key is generated here and deleted in the same layer: an
# image must not carry a private key that every container from it shares. A
# worker container makes its own when it starts (see pacman-keyring.sh); the
# hybrid builds nothing in a chroot of its own -- each build runs in a builder
# container -- so it does without one at runtime, as the official Arch Linux
# image does.
COPY --chmod=0755 docker/pacman-keyring.sh /usr/local/bin/aurcache-pacman-keyring
RUN --mount=type=cache,target=/var/cache/pacman/pkg,id=pacman-runtime-${TARGETPLATFORM} \
    sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf \
    && pacman -Syu --noconfirm --needed \
    && pacman-key --init \
    && pacman-key --populate \
    && aurcache-pacman-keyring forget \
    && systemd-machine-id-setup

# What each image runs -- the binaries, the users, the directories and their
# modes -- installs from the same packages a native install uses. That is the
# point of packaging first: the image stops being a second, hand-maintained
# copy of the host contract that can drift from the documented one. The
# packages are bind-mounted from the packager rather than copied, so no layer
# carries them. See install-packages.sh for the sync and the keyring.
COPY --chmod=0755 docker/install-packages.sh /usr/local/bin/aurcache-install-packages

########## Stage 2b: the worker image ##########
FROM runtime AS worker
ARG TARGETPLATFORM

# The worker's binaries, its two users, the directories and their modes, the
# sudoers entry, the sandbox policy, and the patched makechrootpkg
# (`aurcache-build`). pacman pulls devtools (mkarchroot / makechrootpkg /
# arch-nspawn) and python as ordinary dependencies.
RUN --mount=type=cache,target=/var/cache/pacman/pkg,id=pacman-runtime-${TARGETPLATFORM} \
    --mount=type=bind,from=packager,source=/pkg,target=/tmp/pkg \
    aurcache-install-packages /tmp/pkg aurcache-sandbox aurcache-worker

# The same locations the package's tmpfiles declaration creates. A container
# has no systemd unit to carry them, so they are set here instead.
ENV WORKER_DATA_DIR=/var/lib/aurcache-worker \
    WORKER_CHROOT_DIR=/var/lib/aurcache-worker/chroot

# Persist the identity and the storage pool -- base chroot, builds and caches,
# all in one image under the chroot directory -- even when the worker is
# started with a bare `docker run` and no `-v`. Without this they sit in the
# container's writable layer, so recreating the container (an image bump, a
# config edit) re-enrolls the worker as a new, unapproved machine and rebuilds
# the base chroot from nothing. `docker run` fills these with anonymous volumes;
# a compose file or `-v` still overrides them with named ones.
VOLUME ["/var/lib/aurcache-worker"]

# Wrapper so devtools' systemd-nspawn works without a systemd manager (see
# script). Container-only: a real host has a manager and needs none of this.
COPY --chmod=0755 docker/nspawn-wrapper.sh /usr/local/bin/systemd-nspawn
# Entrypoint fixes shared-enroll-volume ownership before dropping to the worker.
COPY --chmod=0755 docker/worker-entrypoint.sh /usr/local/bin/worker-entrypoint

USER aurcache
WORKDIR /var/lib/aurcache-worker

# Default: enroll and poll for jobs. Override the command for `build-once`.
ENTRYPOINT ["/usr/local/bin/worker-entrypoint"]
CMD ["run"]

########## Stage 2c: the hybrid image ##########
FROM runtime AS hybrid
ARG TARGETPLATFORM

# The server, the sandbox it parses PKGBUILDs through, and the legacy builder.
# pacman pulls alpm-pkgbuild-bridge as an ordinary dependency. Podman (with
# crun and fuse-overlayfs, as upstream's image had) is what DinD mode builds
# in.
#
# The image's own /etc/localtime link goes: a host's bind-mounted over it would
# land on the link's UTC target and be reported as "UTC"; see server.Dockerfile.
RUN --mount=type=cache,target=/var/cache/pacman/pkg,id=pacman-runtime-${TARGETPLATFORM} \
    --mount=type=bind,from=packager,source=/pkg,target=/tmp/pkg \
    aurcache-install-packages /tmp/pkg aurcache-sandbox aurcache-server aurcache-worker-docker \
        -- podman crun fuse-overlayfs \
    && rm -f /etc/localtime /etc/timezone

ENV WORKER_DATA_DIR=/var/lib/aurcache-worker

# Podman configured to run nested, as upstream's `quay.io/podman/stable` base
# came; see the files themselves.
COPY docker/podman-containers.conf /etc/containers/containers.conf
COPY docker/podman-storage.conf /etc/containers/storage.conf
COPY --chmod=0755 docker/hybrid-entrypoint.sh /usr/local/bin/hybrid-entrypoint
COPY --chmod=0755 docker/private-state.sh /usr/local/bin/aurcache-private-state

# State that must outlive the container.
#
# A pre-worker compose file persists the repository (and, with SQLite, the
# database) and nothing else, and this image has to keep building from such a
# file unedited. So while the server's own directories for the worker CA and
# the build logs are not on a volume, it keeps them inside the repository,
# under dot directories that are never served -- the server's file server and
# the compose files' nginx config refuse dot segments, as they do for
# `.staging` -- that the retired-package sweep skips, and that only root can
# read:
#
#   /app/repo/.build_logs  build logs. 0.5.0 kept them in its database; the
#                          upgrade moves them out to files.
#   /app/repo/.ca          the internal CA that signs every worker's
#                          certificate. Losing it orphans every worker that
#                          ever enrolled.
#
# Mount /app, as the split server's compose files do, and the next start moves
# both to where that server keeps them; AURCACHE_CA_DIR and
# AURCACHE_BUILD_LOG_PATH choose other places. That is the server binary's own
# behaviour (aurcache_common::fs::adopt_hybrid_dirs), not this image's, so
# swapping this image for the split server changes nothing but the embedded
# worker.
#
# Anywhere unmounted they would live in the container's own layer, lost with
# it. A leaked CA key cannot authenticate anyone as a worker -- the server
# also requires an approved key -- so the repository volume is no worse a home
# for it than a data directory.

# Kept in anonymous volumes, as losing them is survivable:
#
#   /var/lib/aurcache-worker  the embedded worker's identity. Recreated, the
#                             worker comes back under a new key and the server
#                             re-keys its row (AURCACHE_EMBEDDED_WORKER_SECRET in
#                             hybrid-entrypoint.sh). Not with the CA: unlike the
#                             CA's, this key does authenticate as a worker.
#   /var/lib/containers       Podman's image store in DinD mode, kept off the
#                             container's own overlay filesystem (see
#                             podman-storage.conf), and the builder image with
#                             it, so it is not pulled again on every recreate.
VOLUME ["/var/lib/aurcache-worker", "/var/lib/containers"]

# See server.Dockerfile: whatever the runtime, the server knows it is in a
# container, and so which of its directories are unmounted.
ENV AURCACHE_CONTAINER_IMAGE=hybrid

WORKDIR /app
CMD ["/usr/local/bin/hybrid-entrypoint"]
