---
sidebar_position: 9
---

# Hybrid image

:::warning Deprecated
The `aurcache` image runs the server *and* a build worker in one container. It
exists so that deployments predating build workers keep working after an
upgrade without editing their compose file. It will be removed in a future
release — migrate to `aurcache-server` + `aurcache-worker` when convenient.
:::

If you already run AURCache as a single container, pulling the new `aurcache`
image keeps it building with no changes. On startup it launches the server, then
an embedded worker that enrolls over loopback with a secret generated inside the
container.

The embedded worker is the [legacy container builder](./legacy-docker.md): a
container per package, as before, so the requirements are the ones you already
meet. Which Docker API it builds against is decided from your existing
configuration:

| Your setup | Builds run in |
|---|---|
| `privileged: true` (the old DinD mode) | A Podman running inside the container, as the old image's did |
| `BUILD_ARTIFACT_DIR` set (the old host mode) | Containers on the host, through the mounted Docker socket |
| Neither | Nothing. The server starts and the UI reports that no worker is available. |

`BUILD_ARTIFACT_DIR` is not a hint here — in the old code it *was* the
definition of host build mode, so a deployment that sets it gets the behaviour
it had before.

Packages for other architectures build as before too, through the host's
qemu/binfmt handlers: the image offers every architecture whose handler is
registered (`/proc/sys/fs/binfmt_misc`). `MAX_CONCURRENT_BUILDS` still sets how
many packages build at once.

### What you gain by migrating

- Builds move off the server host, or onto several machines.
- Native workers for other architectures, instead of emulation.
- Routing: reserve particular packages for particular workers, and prefer fast
  workers over slow ones — see [Routing](./routing.md).
- Builds in a clean `devtools` chroot rather than a reused image, with a
  per-build disk quota.
- Source and package caches, so rebuilds do not re-download the world.
- Build credentials for packages whose sources need authentication.

### Migrating

1. Split your single service into two, following the
   [Quick Start](../overview/quick-start.md) — server on the `aurcache-server`
   image, worker on `aurcache-worker`.
2. Keep your existing `/app/repo` and database volumes on the server. Nothing
   about the repository or package history changes.
3. Drop `BUILD_ARTIFACT_DIR`, the Docker socket mount, and the `artifact_cache`
   volume — the worker uploads its packages over the API instead of through a
   shared directory.
4. Move `privileged: true` from the server to the worker, and give the worker a
   tmpfs `/run`.

The split worker has requirements this image does not, Linux 6.7 or newer
among them — see [Requirements](../overview/requirements.md). On a host that
cannot meet them (Synology, for one), stay on this image for now.

The worker your hybrid container was running keeps its history; a new worker
simply enrolls alongside it, and you can retire the old row from the Workers
page — see [Managing workers](./managing.md).
