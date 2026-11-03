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
image keeps it building with no changes — read [Upgrading from
0.5](../overview/upgrading.md) first, for what to back up and what the first
start does. On startup it launches the server, then
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

Packages for other architectures build as before, through the host's
qemu/binfmt handlers (see the
[legacy container builder](./legacy-docker.md#other-architectures)).
`MAX_CONCURRENT_BUILDS` still sets how many packages build at once, and
`CPU_LIMIT` and `MEMORY_LIMIT` each build's share. Set instead on 0.5's settings
page, the three become the embedded worker's settings, on its page under
Workers.

### Keeping its state across recreates

Your compose file persists the repository (and, with SQLite, the database), and
that is all the upgrade needs. What the image adds that must survive goes into
the repository volume while there is no other, under dot directories that are never served and that
only root can read:

| Path | What it holds |
|---|---|
| `/app/repo/.build_logs` | Build logs, including those your old version kept in its database |
| `/app/repo/.ca` | The internal CA that signs every worker's certificate |

That is only while nothing better is mounted. Mount `/app`, or set
`AURCACHE_CA_DIR` or `AURCACHE_BUILD_LOG_PATH` to a directory on a volume, and
the next start moves them there; see [Migrating](#migrating). The split server
image behaves the same way.

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

Everything but the last step happens on this image, one change at a time, with
packages building throughout. The last step only removes the embedded worker.

1. **Give the server all of `/app`**, beside the volumes you have:

   ```yaml
       volumes:
         - ./aurcache/app:/app
         - ./aurcache/repo:/app/repo
         - ./aurcache/db:/app/db   # SQLite only
   ```

   On the next start the server moves the CA and the build logs out of the
   repository, into `/app/data/ca` and `/app/build_logs`, and logs each move. A
   move interrupted by a restart picks up where it stopped. Every enrolled
   worker keeps trusting the server, and every build keeps its log.

2. **Add workers**, following the [Quick Start](../overview/quick-start.md): a
   `builder` service on `aurcache-worker`, the `enroll` volume, and on this
   service `enroll:/enroll:ro`, `AURCACHE_ENROLLMENT_DIR=/enroll` and
   `AURCACHE_TLS_SANS=aurcache`. Publish port 8083 for workers on other hosts.
   They build alongside the embedded worker, and you can watch them do it
   before relying on them alone.

3. **Swap this image for `aurcache-server`**, and drop what only the embedded
   worker used: `privileged: true`, `BUILD_ARTIFACT_DIR`, the Docker socket and
   the build directory mount. The server finds everything where it left it.
   The embedded worker shows as offline; retire its row from the Workers page
   (see [Managing workers](./managing.md)) — its history stays.

Skipping step 1 loses nothing: the server keeps using the repository's
directories, and says so at every start, until `/app` is mounted.

The split worker has requirements this image does not, Linux 6.7 or newer
among them — see [Requirements](../overview/requirements.md). On a host that
cannot meet them (Synology, for one), stay on this image for now.
