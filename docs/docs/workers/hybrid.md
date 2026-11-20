---
sidebar_position: 9
---

# Hybrid image

:::note Compatibility image
The `aurcache` image runs the server *and* a build worker in one container. It
exists so that deployments predating build workers keep working after an
upgrade without editing their compose file. New setups use `aurcache-server` +
`aurcache-worker`.
:::

If you already run AURCache as a single container, pulling the new `aurcache`
image keeps it building with no changes — read [Upgrading from
0.5](../overview/upgrading.md#from-0-5) first, for what to back up and what the first
start does. On startup it launches the server, then
an embedded worker that enrolls over loopback with a secret generated inside the
container.

The embedded worker is the [Docker worker](./docker.md): a
container per package, as before, so the requirements are the ones you already
meet. Which Docker API it builds against is decided from your existing
configuration:

| Your setup | Builds run in |
|---|---|
| `privileged: true` (the old DinD mode) | A Podman running inside the container, as the old image's did |
| The capabilities and overrides in [Without `privileged`](#without-privileged) | The same Podman, in a container that is not privileged |
| `BUILD_ARTIFACT_DIR` set (the old host mode) | Containers on the host, through the mounted Docker socket |
| None of these | Nothing. The server starts and the UI reports that no worker is available. |

`BUILD_ARTIFACT_DIR` is not a hint here — in the old code it *was* the
definition of host build mode, so a deployment that sets it gets the behaviour
it had before.

Packages for other architectures build as before, through the host's
qemu/binfmt handlers (see the
[Docker worker](./docker.md#other-architectures)).
`MAX_CONCURRENT_BUILDS` still sets how many packages build at once, and
`CPU_LIMIT` and `MEMORY_LIMIT` each build's share. Set instead on 0.5's settings
page, the three become the embedded worker's settings, on its page under
Workers.

### Without `privileged`

DinD mode does not need the whole of `privileged: true`. The Podman inside the
container runs with one added capability, two Podman settings and a seccomp
profile, which suits hosts that refuse privileged containers or run Docker with
`userns-remap`. This recipe was worked out in
[#276](https://github.com/Lukas-Heiligenbrunner/AURCache/issues/276) on 0.5's
image, and has been adapted to this one. Read [what it does and does not
protect](#what-it-protects) before choosing it.

```yaml
services:
  aurcache:
    image: ghcr.io/lukas-heiligenbrunner/aurcache:latest
    # instead of `privileged: true`
    cap_add:
      - SYS_ADMIN
    security_opt:
      - seccomp=./podman-seccomp.json
      - apparmor=unconfined
      - label=disable
      # Only if your daemon enables no-new-privileges by default: it would stop
      # `sudo pacman` in every build.
      # - no-new-privileges:false
    volumes:
      - ./aurcache/db:/app/db
      - ./aurcache/repo:/app/repo
      # Podman's image store. It must not sit on the container's own overlay
      # filesystem; the image declares it a volume, naming it keeps the builder
      # image across recreates.
      - containers:/var/lib/containers
      - ./podman-unprivileged.conf:/etc/containers/containers.conf.d/50-unprivileged.conf:ro
      - ./podman-native-overlay.conf:/etc/containers/storage.conf.d/50-native-overlay.conf:ro

volumes:
  containers:
```

The two drop-ins are added to the image's own Podman configuration, not
replacing it. `podman-unprivileged.conf`:

```toml
[containers]
# /sys/fs/cgroup is read-only in a container that is not privileged, so Podman
# cannot create a cgroup per build.
cgroups = "disabled"
```

`podman-native-overlay.conf`:

```toml
[storage.options.overlay]
# The image mounts its layers with fuse-overlayfs, which needs /dev/fuse.
# Use the kernel's overlay instead.
mount_program = ""
mountopt = "nodev"
```

`podman-seccomp.json` is the profile Podman itself applies, taken from the
image you run:

```sh
docker run --rm --entrypoint cat ghcr.io/lukas-heiligenbrunner/aurcache:latest \
    /usr/share/containers/seccomp.json > podman-seccomp.json
```

Docker's default profile refuses `pivot_root` and the kernel keyring calls, both
of which Podman uses to start a build. Podman's own profile allows them and
still filters the rest.

What each piece is for:

- **`SYS_ADMIN`** lets Podman mount the build's layers and create its
  namespaces. The reporter also needed `MKNOD` with Podman as the outer runtime;
  rootless Podman did not need it here.
- **`apparmor=unconfined`**: Docker's default AppArmor profile denies `mount`.
  Without AppArmor on the host it changes nothing.
- **`label=disable`**: the same for SELinux.
- **`/var/lib/containers`** must be on a filesystem overlay can use as its upper
  layer: not overlay itself, and on ZFS only OpenZFS 2.2 or newer. Where it
  cannot be, keep fuse-overlayfs instead: leave out `podman-native-overlay.conf`
  and add `devices: [/dev/fuse]`.
- **The kernel**: mounting overlay from inside a user namespace (rootless
  Docker or Podman, `userns-remap`) needs Linux 5.11 or newer; the original
  report needed 5.12.

The rest of the original recipe is already the image's configuration: Podman
on cgroupfs with `crun`, file-based events, `k8s-file` logs, and each build
sharing the container's network, user and cgroup namespaces. The image deliberately gives each build
its own IPC and UTS namespaces; a build needs neither of the container's.

#### If you cannot ship a seccomp profile

Docker's default profile can stay if Podman avoids the two calls it refuses.
Leave out the `seccomp=` line and add to `podman-unprivileged.conf`:

```toml
[containers]
cgroups = "disabled"
keyring = false

[engine]
no_pivot_root = true
```

`no_pivot_root` makes the runtime enter each build's root filesystem with
`chroot` rather than `pivot_root`, and a build has root in its container
through `sudo`. Escaping a `chroot` as root is well known, and on the other
side is this container: the server's database, its CA key and the repository.
Prefer the seccomp profile.

#### Switching an existing deployment

A store created with fuse-overlayfs keeps using it whatever the configuration
says, and then fails on the missing `/dev/fuse`. Give `/var/lib/containers` a
fresh volume when you switch. It only holds the builder image, which the next
build pulls again.

These are the errors a missing piece produces, in the build's log or the
container's:

| Error | Missing |
|---|---|
| `fuse: device /dev/fuse not found` | `podman-native-overlay.conf`, or an old store (see above) |
| `join keyctl ...: Operation not permitted` | the seccomp profile, or `keyring = false` |
| `pivot_root: Operation not permitted` | the seccomp profile, or `no_pivot_root = true` |
| `cgroup.subtree_control ...: Read-only file system` | `cgroups = "disabled"` |
| `The "no new privileges" flag is set` | `no-new-privileges:false` |

#### Resource limits

With no cgroup per build, `CPU_LIMIT` and `MEMORY_LIMIT` are **not applied**,
and nothing says so: Podman accepts the limit and ignores it. Limit the
container itself instead (`mem_limit`, `cpus`), which caps the server and every
build together.

#### What it protects

- **It removes the accidental exposure.** The container no longer sees the
  host's devices and cannot load kernel modules.
- **On its own it is not a boundary.** `CAP_SYS_ADMIN` in the host's user
  namespace is close to root: it can mount and enter namespaces, and with
  AppArmor off nothing narrows that. Without a user namespace, root in the
  container is root on the host, and something that gets there can likely
  reach the rest of the host.
- **Under rootless Docker or Podman, or `userns-remap`, it is one.** Root in
  the container is then an unprivileged range of host ids, and its capabilities
  only apply to namespaces the container owns. Getting from there to the host
  takes a kernel bug, not a missed setting.
- **The server shares the container.** A build that escapes its Podman
  container is in the server's container, with its keys and its database,
  whatever surrounds it. That is true with `privileged: true` as well, and is
  one more reason to [migrate](#migrating): the split worker builds away from
  the server.

[Running workers without host root](https://github.com/Lukas-Heiligenbrunner/AURCache/blob/master/design/proposed/unprivileged-workers.md)
looks at the alternatives in more depth.

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
- A network policy on builds. Here a build shares the server's network
  namespace and reaches the API at `localhost:8080`, which without
  [OAuth](../Configuration/authentication.md) lets it do anything an admin
  can; a chroot worker
  [keeps its builds off the server and the local network](./split-chroot.md#what-builds-can-reach-on-the-network).

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
