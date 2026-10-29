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

Packages for other architectures build through the host's qemu/binfmt
handlers, as before, for the architectures listed in `WORKER_EMULATED_ARCHES`
(see the [legacy container builder](./legacy-docker.md#other-architectures)).
`MAX_CONCURRENT_BUILDS` still sets how many packages build at once.

### Keeping its state across recreates

Your compose file persists the repository (and, with SQLite, the database), and
that is all the upgrade needs. What the image adds that must survive goes into
the repository volume, under dot directories that are never served and that
only root can read:

| Path | What it holds |
|---|---|
| `/app/repo/.build_logs` | Build logs, including those your old version kept in its database |
| `/app/repo/.ca` | The internal CA that signs every worker's certificate |

If you copy the repository somewhere else to serve it — `rsync` to a mirror,
say — exclude dot directories, as you would `.staging`.

The embedded worker keeps its own identity in an anonymous volume. When that is
lost — `docker compose down`, or an orchestrator such as TrueNAS that stops the
app before every start — the worker comes back with a new key and the server
gives its existing row the new key: same history, same settings, same approval.
Nothing to do.

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
3. Set `AURCACHE_CA_DIR=/app/repo/.ca` and
   `AURCACHE_BUILD_LOG_PATH=/app/repo/.build_logs` on the server, where this
   image keeps them. Without them the split server creates a new CA, which
   every worker you enrolled would stop trusting, and finds none of the logs
   written so far.
4. Drop `BUILD_ARTIFACT_DIR`, the Docker socket mount, and the `artifact_cache`
   volume — the worker uploads its packages over the API instead of through a
   shared directory.
5. Move `privileged: true` from the server to the worker, and give the worker a
   tmpfs `/run`.

The split worker has requirements this image does not, Linux 6.7 or newer
among them — see [Requirements](../overview/requirements.md). On a host that
cannot meet them (Synology, for one), stay on this image for now.

The worker your hybrid container was running keeps its history; a new worker
simply enrolls alongside it, and you can retire the old row from the Workers
page — see [Managing workers](./managing.md).
