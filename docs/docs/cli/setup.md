---
sidebar_position: 2
---

# Setup and health

## Standing something up

`setup` needs no server and no token — it is the command for when you have
neither. On a machine with Docker:

```bash
aurcli setup server      # the backend, on this machine (docker run)
aurcli setup worker      # one build worker beside it
aurcli doctor            # check they found each other
```

The worker approves itself. `setup` gives the pair a shared `enroll` volume and
network, the same trick the bundled compose file uses, so there is no approval
step and no secret to configure. Add `--dry-run` to either command to print the
`docker run` line instead of running it, and put anything after `--` to pass it
to `docker run` before the image.

More workers on the same machine each need their own container name and
identity volume. The volume defaults to one named after the container
(`worker-2` gets `worker_2_data`):

```bash
aurcli setup worker --container-name worker-2
```

A worker on *other* hardware joins over the network, where trust has to be
explicit — pin the server's CA fingerprint from its startup log line
`Worker CA fingerprint (pin this on workers): <sha256>`:

```bash
aurcli setup worker \
  --server-url https://aurcache.example.com:8083 \
  --ca-fingerprint <sha256> \
  --arch aarch64
```

It then waits on the Workers page for approval, unless it shares an
`--enrollment-token` with the server's `AURCACHE_ENROLLMENT_TOKEN` or the server
pre-approves its fingerprint. Without a fingerprint or a token, `setup worker`
warns that the worker will trust whatever answers on first contact.

The worker keeps its chroots, builds and caches in a [storage
pool](../workers/storage-pool.md) with a disk quota. `setup worker` asks what
should back it: an image file in its volume (nothing to prepare), a block device
such as a zvol, or a dedicated btrfs filesystem. `--pool-image`,
`--pool-device PATH` or `--pool-mount PATH` answer without asking, and
`--disk-max 500G` sets its size.

### A compose file instead

Anything that takes a compose file — TrueNAS, Portainer, Unraid, or
`docker compose up` on any host — gets one:

```bash
aurcli setup compose --role bundle    # server + a local worker (the default)
aurcli setup compose --role backend   # server alone
aurcli setup compose --role worker    # a worker for another machine
```

It writes `docker-compose.yaml` for the bundle, `docker-compose.backend.yaml` or
`docker-compose.worker.yaml` for the other roles, or the file `-o` names; `-o -`
writes to stdout, to paste into a web UI. It refuses to overwrite an existing
file without `--force`. The output keeps the comments explaining why the worker
needs `privileged` and what the `enroll` volume is for, because those are the
parts worth reading before you deploy it.

A file with a worker asks what backs its storage pool, as `setup worker` does,
or takes the same pool flags. On TrueNAS, answer with a zvol:
`--pool-device /dev/zvol/<pool>/<name>`. A worker file takes `--server-url` and
`--ca-fingerprint`, as `setup worker` does; without them it holds
`https://aurcache.example.com:8083` and a fingerprint placeholder, and the worker
will not trust the server until the placeholder is filled in or removed.

A file with a server asks which database it should use, or takes
`--database postgres` or `--database sqlite` (required when there is no terminal
to ask on). PostgreSQL adds an `aurcache_database` service pinned to
`postgres:18-trixie`, with a healthcheck the server waits on before it runs its
migrations. Its password is generated unless you pass `--db-password`, and since
the database publishes no port, it only has to match between the two services.

It also asks whether to add an upgrade step, or takes `--postgres-upgrade` or
`--no-postgres-upgrade`. With no terminal to ask on, the step is left out. The
step is `ixsystems/postgres-upgrade`, the one TrueNAS's own apps use, and runs
before the database and exits. It does nothing while the data already matches
the database's major version. After you raise `TARGET_VERSION` and the image tag
together, it backs the data up and runs `pg_upgrade`. It also moves data from the
older layout (mounted straight at `/var/lib/postgresql/data`) into the one the
file uses. Without the step, changing the image's major version starts an empty
database beside the old data, and upgrading is up to you.

## Consuming the repository

```bash
aurcli repo config                     # print the pacman.conf stanza
aurcli repo config --install           # append it (safe to run twice)
sudo pacman -Sy
```

With a token configured, it asks the server how it actually publishes the
repository rather than assuming the default port. See [Pacman
Repository](../Configuration/pacman-repo.md).

## Health

```bash
aurcli health
aurcli doctor            # server → token → fleet → queue, exits non-zero on failure
```

`doctor` walks the chain and presents the `WaitingReason` the server already
computes — see [Why is a build not
starting](../workers/managing.md#why-is-a-build-not-starting).
`aurcli builds watch` follows the queue live; `pkg add --wait` waits for what
one trigger queues.
