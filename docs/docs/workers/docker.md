---
sidebar_position: 7
---

# Docker worker

The Docker worker builds each package in a container it creates from a builder
image, against a Docker or Podman socket it is given. It is for hosts that
already run a container engine and have no use for a `devtools` chroot, and it
is the builder the [hybrid image](./hybrid.md) embeds.

It builds in a reused image rather than a clean chroot, so it supports neither
the source and package caches nor [build credentials](./credentials.md). The
[chroot worker](./split-chroot.md) offers both.

## How it builds

For each job the worker prepares a build directory, creates a container from
the builder image with the engine's resource limits, streams the container's
output into the build log, and uploads the packages the build leaves in the
directory. The builder image needs pacman, makepkg and gpg.

With `BUILD_ARTIFACT_DIR` set the worker builds against the mounted Docker
socket: the directory must be a path the Docker daemon can bind, with the same
directory visible to the worker. Without it, the [hybrid image](./hybrid.md)
builds against a Podman it runs inside its own container.

`WORKER_BUILD_AGENT` names the build agent the worker places in each build
directory to run the build.

## Settings

Its tunables are `CPU_LIMIT` (milli-CPUs) and `MEMORY_LIMIT` (MB, negative for
unlimited). Like the chroot worker's settings they are listed on the worker's
page and can be set there, and read `CPU_LIMIT_DEFAULT` and
`MEMORY_LIMIT_DEFAULT` as defaults AURCache may override; the plain names still
pin. Its builder image and build directory are not settable from AURCache.

The **Type** column on the Workers page tells the strategies apart: `chroot`
for the chroot worker, `docker` for this one — see
[Managing workers](./managing.md#build-strategies).

## Other architectures

It builds for every platform AURCache supports: each build container is
created from the builder image for the package's platform, and a foreign one
runs through the host's qemu/binfmt handlers. Without a handler for a
platform, its builds fail; register one
(`docker run --privileged --rm tonistiigi/binfmt --install all`) or set
`WORKER_ARCHES` to the platforms this host can build, comma separated
(`x86_64`, `aarch64`, `armv7h`).

It competes for those builds like any worker that builds them, so give a
faster one a higher [priority](./routing.md), or pause this one, to keep it
from taking them.

A [chroot worker](./split-chroot.md) builds for its own architecture only, and
for another as the worker image run for that platform.
