---
sidebar_position: 3
---

# Quick Start

AURCache runs as two pieces: the **server**, which holds the package database
and serves the web UI and the pacman repository, and one or more **build
workers**, which build packages and upload the results. The bundled compose file
starts one of each on a single host.

```bash
curl -O https://raw.githubusercontent.com/Lukas-Heiligenbrunner/AURCache/master/compose/docker-compose.yaml
docker compose up -d
```

- Web UI / API: `http://localhost:8080` (plain HTTP; front it with a reverse proxy for TLS)
- Pacman repo: `http://localhost:8081` (add as a `[repo] Server` in `pacman.conf`)

The worker enrolls itself and starts polling for jobs within a few seconds — no
token and no approval click. For more build throughput, raise the worker's
concurrency on its page in the web UI (or `WORKER_CONCURRENCY_DEFAULT`): each
build already uses every core, so concurrency buys builds at once, not faster
builds.

## With PostgreSQL

SQLite is fine for trying AURCache out, but PostgreSQL is recommended for
anything you intend to keep:

```yaml
services:
  aurcache:
    image: ghcr.io/lukas-heiligenbrunner/aurcache-server:latest
    ports:
      - "8080:8080"   # Web UI + API (plain HTTP; front with a reverse proxy for TLS)
      # No 8081 here: the `repo` service below publishes nginx on that host
      # port instead. Rocket's builtin file handler keeps listening on 8081
      # inside this container, unpublished.
      - "8083:8083"   # Worker protocol (HTTPS + mutual TLS)
    volumes:
      - ./aurcache/repo:/app/repo
      - aurcache_ca:/app/data/ca   # internal worker CA; persist across restarts
      - enroll:/enroll:ro
    environment:
      - AURCACHE_TLS_SANS=aurcache,localhost
      - AURCACHE_PUBLIC_URL=http://localhost:8081
      - AURCACHE_ENROLLMENT_DIR=/enroll
      - DB_TYPE=POSTGRESQL
      - DB_USER=aurcache
      - DB_PWD=YOUR_SECURE_PWD
      - DB_HOST=dbhost
    networks:
      aurcache_network:
    restart: unless-stopped

  builder:
    image: ghcr.io/lukas-heiligenbrunner/aurcache-worker:latest
    depends_on:
      - aurcache
    environment:
      - AURCACHE_URL=https://aurcache:8083
      - AURCACHE_ENROLLMENT_DIR=/enroll
      - WORKER_CONCURRENCY_DEFAULT=2
    volumes:
      - enroll:/enroll
      - worker_data:/var/lib/aurcache-worker
    # Each package is built in a systemd-nspawn chroot, which needs mounts and
    # namespaces a plain container forbids.
    privileged: true
    tmpfs:
      - /run
    networks:
      aurcache_network:
    restart: unless-stopped

  repo:
    # The pacman repository, served statically at line rate: nginx uses
    # sendfile while Rocket's builtin handler streams 4 KiB chunks through
    # userspace. Mounts the repository read-only; the server remains the only
    # writer. Fetch the conf from the repo next to the compose file:
    # curl -O https://raw.githubusercontent.com/Lukas-Heiligenbrunner/AURCache/master/docker/repo-nginx.conf
    image: nginx:alpine
    ports:
      - "8081:80"   # Pacman repository (plain HTTP, for `pacman -Sy`)
    volumes:
      - ./aurcache/repo:/app/repo:ro
      - ./repo-nginx.conf:/etc/nginx/conf.d/default.conf:ro
    depends_on:
      - aurcache
    networks:
      aurcache_network:
    restart: unless-stopped

  aurcache_database:
    # Pin both the major version and the Debian release. A new major version
    # will not start on the old one's data directory, and a new Debian release
    # changes how text sorts under indexes already built.
    image: postgres:18-trixie
    volumes:
      # The parent of PGDATA, not PGDATA itself: since 18 the data lives in
      # <major>/docker below it, so an upgrade builds the new directory beside the old.
      - ./aurcache/db:/var/lib/postgresql
    environment:
      - POSTGRES_PASSWORD=YOUR_SECURE_PWD
      - POSTGRES_USER=aurcache
    restart: unless-stopped
    networks:
      aurcache_network:
        aliases:
          - "dbhost"

volumes:
  aurcache_ca:
  enroll:
  worker_data:

networks:
  aurcache_network:
    driver: bridge
```

The server and the worker authenticate to each other with mutual TLS on port
8083. Sharing the `enroll` volume is what tells the server this worker is
trusted, so no secret has to be configured. For a worker on another machine —
where there is no shared volume — see
[Build Workers](../workers/split-chroot.md).

Keep the Postgres image pinned as shown rather than `postgres:latest` or even
`postgres:18`. Those tags move to a newer Debian release from time to time, and
a database whose indexes were sorted by the old C library then needs its
indexes rebuilt. AURCache checks for that at startup and logs the commands to
run if it finds one (`REINDEX DATABASE` then `ALTER DATABASE ... REFRESH
COLLATION VERSION`, in each database the warning names). Moving to a new major
version of Postgres is a separate upgrade, with `pg_upgrade` or a dump and
restore.

## Or let the CLI do it

If you have `aurcli` (`cargo install aurcache-cli`), it can stand the whole
thing up. Nothing here needs a token or a running server — it is the command for
when you have neither:

```bash
aurcli setup server      # the backend, on this machine
aurcli setup worker      # one build worker beside it
aurcli doctor            # check they found each other
```

The worker approves itself. `setup` gives the pair a shared `enroll` volume, the
same trick the compose file above uses, so there is no approval step and no
secret to configure.

Add `--dry-run` to either command to print the `docker run` line instead of
running it.

More workers on the same machine each need their own name and identity volume:

```bash
aurcli setup worker --container-name worker-2
```

The worker keeps its chroots, builds and caches in a
[storage pool](../workers/storage-pool.md) with a disk quota. `setup worker`
asks what should back it: an image file in its volume (nothing to prepare), a
block device such as a zvol, or a dedicated btrfs filesystem. `--pool-image`,
`--pool-device PATH` or `--pool-mount PATH` answer without asking, and
`--disk-max 500G` sets its size.

A worker on *other* hardware joins over the network, where trust has to be
explicit — pin the server's CA fingerprint from its startup log:

```bash
aurcli setup worker \
  --server-url https://aurcache.example.com:8083 \
  --ca-fingerprint <sha256> \
  --arch aarch64
```

### For TrueNAS, Portainer, Unraid…

Anything that takes a compose file gets one:

```bash
aurcli setup compose --role bundle    # server + a local worker
aurcli setup compose --role backend   # server alone
aurcli setup compose --role worker    # a worker for another machine
```

`-o -` writes to stdout to paste into a web UI; otherwise it writes
`docker-compose.yaml` (or `docker-compose.<role>.yaml`) and refuses to clobber an
existing file without `--force`. The output keeps the comments explaining why the
worker needs `privileged` and what the `enroll` volume is for, because those are
the parts worth reading before you deploy it. A file with a worker asks what
backs its storage pool, as `setup worker` does. On TrueNAS, answer with a zvol:
`--pool-device /dev/zvol/<pool>/<name>`.

A file with a server asks which database it should use, or takes
`--database postgres` or `--database sqlite` (required when there is no terminal
to ask on). PostgreSQL adds a database service pinned to `postgres:18-trixie`.
Its password is generated unless you pass `--db-password`, and since the database
publishes no port, it only has to match between the two services.

It also asks whether to add an upgrade step, or takes `--postgres-upgrade` or
`--no-postgres-upgrade`. With no terminal to ask on, the step is left out. The
step is `ixsystems/postgres-upgrade`, the one TrueNAS's own apps use, and runs
before the database and exits. It does nothing while the data already matches
the database's major version. After you raise `TARGET_VERSION` and the image tag
together, it backs the data up and runs `pg_upgrade`. It also moves data from the
older layout (mounted straight at `/var/lib/postgresql/data`) into the one the
file uses. Without the step, changing the image's major version starts an empty
database beside the old data, and upgrading is up to you.

## Filling it, and using it

A fresh instance is empty. If the machine you are on already installs packages
from the AUR, that is the set you probably want mirrored, and `pacman -Qm` is
what lists it:

```bash
aurcli pkg add --from-installed
```

It prints what it found and asks before submitting — adding a package resolves
its dependencies and enqueues builds for them, so a long list is not something
to send unseen. Pass `--yes` to skip the prompt in a script, where it is
otherwise refused rather than assumed.

Once something has built, wire the repository into pacman:

```bash
aurcli repo config | sudo tee -a /etc/pacman.conf
sudo pacman -Sy
```

If a build does not start, [`aurcli doctor`](../workers/managing.md#why-is-a-build-not-starting)
says why.

`aurcli completions <shell>` prints a completion script for bash, zsh,
fish, elvish or powershell.

## Upgrading from a single-container setup

If you already run AURCache as one container, it keeps working: see
[hybrid compatibility image](../workers/hybrid.md).

For more advanced setup see the
[Configuration](/docs/Configuration/environment-variables) page.
