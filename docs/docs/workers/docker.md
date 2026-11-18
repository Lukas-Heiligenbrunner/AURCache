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

## Running it by itself

Outside the hybrid image the worker is one program, `aurcache-worker-docker`,
from the AUR package of that name. The package installs the program and the
`aurcache` user and directories, and no service unit: start it from a unit of
your own, as a user that may use the engine's socket (`/var/run/docker.sock`,
or the Unix socket `DOCKER_HOST` names). There is no image of it alone.

It connects to AURCache and is approved like the chroot worker, with the same
variables: [Connecting to AURCache](./split-chroot.md#connecting-to-aurcache),
[Identity and capabilities](./split-chroot.md#identity-and-capabilities), and
`WORKER_DATA_DIR` for its key. These are its own:

| Variable | Description | Default |
|---|---|---|
| `BUILD_ARTIFACT_DIR` | The build directory as the engine's daemon sees it: the path it binds into each build container. The worker does not start without it | none |
| `BUILD_ARTIFACT_DIR_LOCAL` | The same directory as the worker sees it | `builds` under the worker's working directory |
| `BUILDER_IMAGE` | The image builds run in | `ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest` |
| `AURCACHE_BUILDER_NETWORK` | The Docker network build containers join, or `host` | the network namespace of the worker's own container |

Set both directory variables, to the same path when the worker and the daemon
run on one host. `BUILD_ARTIFACT_DIR_LOCAL` does not default to
`BUILD_ARTIFACT_DIR`: left unset, the worker writes each job under its working
directory while the daemon binds the other path, and every build starts in an
empty directory. In the hybrid image that default is `/app/builds`, which is
where its compose files mount the host's directory.

A build installs its dependencies from AURCache's repository at the address
the worker itself uses, so that address has to mean the same thing inside a
build container. The default arranges that for a worker that runs in a
container. For one that runs on the host, set `AURCACHE_BUILDER_NETWORK=host`,
or name a network on which `AURCACHE_REPO_URL` resolves.

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

A build runs `sudo` (pacman, to install its dependencies), so a foreign
platform also needs its handler registered with the `C` flag, or `sudo` stops
with `effective uid is not 0`: qemu has to run it with the binary's own setuid
credentials. `tonistiigi/binfmt` sets it; Arch's `qemu-user-static-binfmt`
does not, and a copy of its `/usr/lib/binfmt.d/qemu-<arch>-static.conf` in
`/etc/binfmt.d/` with the flags `FP` changed to `FPC` does, at the next boot or
`systemctl restart systemd-binfmt`.

It competes for those builds like any worker that builds them, so give a
faster one a higher [priority](./routing.md), or pause this one, to keep it
from taking them.

A [chroot worker](./split-chroot.md) builds for its own architecture only, and
for another as the worker image run for that platform.
