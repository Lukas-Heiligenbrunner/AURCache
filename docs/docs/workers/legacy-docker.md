---
sidebar_position: 7
---

# Legacy container builder

:::warning Deprecated
The legacy container builder exists so deployments predating build workers keep
building after an upgrade. It is provided for continuity, not as a supported
build strategy — migrate to the [split chroot worker](./split-chroot.md) when
convenient.
:::

The legacy builder runs each package in a container spawned from a reused
builder image, against a mounted Docker socket, instead of in a clean chroot.
It is the more limited of the two strategies: it builds in a reused image
rather than a clean chroot, and supports neither the source and package caches
nor [build credentials](./credentials.md).

It is the [hybrid image](./hybrid.md)'s builder. With `BUILD_ARTIFACT_DIR` set
— in the old code that variable *was* the definition of host build mode — it
builds against the mounted Docker socket, and the directory must be a path the
Docker daemon can bind, with the same directory visible to the container.
Without it, it builds against a Podman the hybrid image runs inside its own
container, as the old DinD mode did.

Its only tunables are `CPU_LIMIT` (milli-CPUs) and `MEMORY_LIMIT` (MB, negative
for unlimited). Like the chroot worker's settings they are listed on the
worker's page and can be set there, and read `CPU_LIMIT_DEFAULT` and
`MEMORY_LIMIT_DEFAULT` as defaults AURCache may override; the plain names still
pin. Its builder image and build directory are not settable from AURCache.

The **Type** column on the Workers page tells the strategies apart: `chroot`
for the supported worker, `docker` (highlighted) for this one — see
[Managing workers](./managing.md#build-strategies).

## Other architectures

It builds for every platform AURCache supports, as the old builder did: each
build container is created from the builder image for the package's platform,
and a foreign one runs through the host's qemu/binfmt handlers. Without a
handler for a platform, its builds fail, as they always did; register one
(`docker run --privileged --rm tonistiigi/binfmt --install all`) or set
`WORKER_ARCHES` to the platforms this host can build, comma separated
(`x86_64`, `aarch64`, `armv7h`).

It competes for those builds like any worker that builds them, so give a
faster one a higher [priority](./routing.md), or pause this one, to keep it
from taking them.

A [chroot worker](./split-chroot.md) builds for its own architecture only, and
for another as the worker image run for that platform.
