---
sidebar_position: 2
---

# Docker Compose setup

AURCache runs as two pieces:

- the **server** (`aurcache-server`) — package database, web UI, API, and the
  pacman repository;
- one or more **build workers** (`aurcache-worker`) — each builds packages in a
  clean `devtools` chroot and uploads the results.

They talk over mutual TLS on port 8083, so a worker can live in the same compose
stack, on another machine, or on another architecture.

See [Docker images](./artifacts/docker-images.md) for the images and tags.

## Single host

The bundled [`docker-compose.yaml`](https://github.com/Lukas-Heiligenbrunner/AURCache/blob/master/compose/docker-compose.yaml)
starts a server plus one local worker and needs no edits:

```bash
curl -O https://raw.githubusercontent.com/Lukas-Heiligenbrunner/AURCache/master/compose/docker-compose.yaml
docker compose up -d
```

The worker auto-enrolls through the shared `enroll` volume — being able to write
to a volume the server reads is what proves it is trusted, so there is no token
to configure and no approval to click.

See the [Quick Start](../overview/quick-start.md#with-postgresql) for the same
setup with PostgreSQL.

## What to persist

Everything the server keeps lives under `/app`, so one mount covers all of it:

```yaml
volumes:
  - aurcache_data:/app
```

That is the database, the package repository, the internal worker CA, each
build's log and the source cache. Mounting the pieces individually works and is
the right thing when they belong on different storage — a large repository on
bulk disks, the database on an SSD — but then **anything you do not mount lives
in the container and is gone when the container is replaced**, which happens on
every image update. Build logs were lost that way for a long time, because
`/app/build_logs` was in nobody's volume list.

Both shapes can be combined: a mount for `/app` and a more specific one for a
subdirectory of it are applied parent-first regardless of the order you list
them.

```yaml
volumes:
  - aurcache_data:/app          # everything by default
  - big_pool:/app/repo          # …except the packages, which live elsewhere
```

One catch when adding a specific mount to a deployment that has been running:
the inner mount **shadows** whatever the outer volume holds at that path rather
than adopting it. Packages already in `aurcache_data/repo` would still be there,
but the server would no longer see them. Copy them into the new volume before
switching, or it looks exactly like data loss.

## Why the worker needs `privileged`

Each package is built in its own `systemd-nspawn` chroot, which needs mounts and
namespaces an ordinary container forbids. `privileged: true` plus a tmpfs `/run`
is the tightest configuration that works everywhere; if your host allows it you
can narrow this to specific capabilities with seccomp/apparmor unconfined.

Narrowing it hides the host's other devices from the worker, which matters most
on a NAS, but it is not a security boundary: `SYS_ADMIN` in the host's user
namespace, with seccomp and AppArmor off, is close to root on the host. A user
namespace would be one, but the chroot worker does not run in one yet: its
storage pool needs a loop device and a btrfs mount, and `systemd-nspawn` has not
been shown to work there. To keep a misbehaving build or worker off the host,
run the worker in a virtual machine and enroll it as a [remote
worker](#more-workers). The options and what has been tried are in [Running
workers without host
root](https://github.com/Lukas-Heiligenbrunner/AURCache/blob/master/design/proposed/unprivileged-workers.md),
a proposal not yet implemented. The [hybrid image](../workers/hybrid.md#without-privileged)'s
container builder can run without `privileged`, with the same caveat.

The worker also runs in the host's cgroup namespace (`cgroup: host`,
`docker run --cgroupns=host`). Its [network
policy](../workers/split-chroot.md#what-builds-can-reach-on-the-network)
matches each build by its cgroup, and a kernel older than 6.11, or a stable
kernel without the fix, cannot match from a container's own cgroup namespace;
the worker then takes no builds. A privileged container gains nothing from it
that it could not take anyway; the build user reading a package's
`.SRCINFO` can see the host's whole cgroup tree, its services' names and
memory figures, as it can on a native install.

Only the **worker** needs this. The server handles no build payloads and runs
unprivileged.

## Where the worker keeps its builds

Everything a worker stores (the base chroot, each build, its caches) lives in
its [storage pool](../workers/storage-pool.md), a btrfs filesystem with a disk
quota per build and one over the whole worker. By default the pool is a sparse
image file in the worker's `/var/lib/aurcache-worker` volume, and needs no
setup. On ZFS, such as TrueNAS, back it with a zvol instead. Pass the zvol
through as a device and name it in `WORKER_POOL`:

```yaml
  builder:
    environment:
      - WORKER_POOL=/dev/aurcache-pool
      - WORKER_DISK_MAX_DEFAULT=950G
    devices:
      - /dev/zvol/tank/aurcache-worker:/dev/aurcache-pool
```

## Generating these files

`aurcli setup compose --role bundle|backend|worker` writes the file for each of
these topologies, with the same comments and the same defaults, and needs no
server and no token. `aurcli setup server` and `aurcli setup worker` skip the
file and run the containers directly. See [Setup and
health](../cli/setup.md#a-compose-file-instead).

## More workers

For more build throughput, raise the worker's concurrency on its page in the
web UI (or `WORKER_CONCURRENCY_DEFAULT`): each build already uses every core,
so concurrency buys builds at once, not faster builds.

Workers on separate hardware, or on a foreign architecture, are covered in
[Build Workers](../workers/split-chroot.md) and
[`docker-compose.remote-worker.yaml`](https://github.com/Lukas-Heiligenbrunner/AURCache/blob/master/compose/docker-compose.remote-worker.yaml).

If you still run AURCache as one container, see the [hybrid compatibility
image](../workers/hybrid.md).
