---
sidebar_position: 2
---

# Setup and health

## Standing something up

`setup` needs no server and no token — it is the command for when you have
neither:

```bash
aurcli setup server      # the backend, on this machine (docker run)
aurcli setup worker      # one build worker beside it
aurcli doctor            # check they found each other
```

The worker approves itself. `setup` gives the pair a shared `enroll` volume,
the same trick the bundled compose file uses, so there is no approval step and
no secret to configure. Add `--dry-run` to either command to print the
`docker run` line instead of running it.

More workers on the same machine each need their own name and identity volume:

```bash
aurcli setup worker --container-name worker-2
```

A worker on *other* hardware joins over the network, where trust has to be
explicit — pin the server's CA fingerprint from its startup log:

```bash
aurcli setup worker \
  --server-url https://aurcache.example.com:8083 \
  --ca-fingerprint <sha256> \
  --arch aarch64
```

`setup worker` asks what should back the worker's [storage
pool](../workers/storage-pool.md): an image file in its volume (nothing to
prepare), a block device such as a zvol, or a dedicated btrfs filesystem.
`--pool-image`, `--pool-device PATH` or `--pool-mount PATH` answer without
asking, and `--disk-max 500G` sets its size.

Anything that takes a compose file gets one instead:

```bash
aurcli setup compose --role bundle    # server + a local worker
aurcli setup compose --role backend   # server alone
aurcli setup compose --role worker    # a worker for another machine
```

`-o -` writes to stdout to paste into a web UI (TrueNAS, Portainer, Unraid);
otherwise it writes `docker-compose.yaml` and refuses to clobber an existing
file without `--force`. `--database postgres|sqlite` chooses the server's
database, and `--[no-]postgres-upgrade` whether PostgreSQL gets an upgrade
step. See [Quick Start](../overview/quick-start.md#for-truenas-portainer-unraid).

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
