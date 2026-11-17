---
sidebar_position: 4
---

# Upgrading from 0.5

An instance set up from the 0.5 docs upgrades by changing the image line and
nothing else: the `aurcache` image is now the [hybrid
image](../workers/hybrid.md), the server and a worker that builds the way 0.5
did. This page is what to do before, and what to expect after.

## Before

**Back up.** The upgrade converts the database once and cannot be undone: 0.5
refuses to start on what it leaves. Going back means restoring a copy.

- PostgreSQL: dump it while 0.5 is still running —
  `docker compose exec aurcache_database pg_dump -U aurcache postgres > aurcache-0.5.sql`
  (the user and database are `DB_USER` and `DB_NAME`, `postgres` unless you set
  it).
- SQLite: the server copies the database to `db.sqlite.pre-upgrade`, beside it
  in `./aurcache/db`, before converting it. Copying the directory yourself costs
  nothing either.
- The repository (`./aurcache/repo`) is only added to, but a copy is the only
  way back to exactly what clients saw.

**Pin PostgreSQL.** The 0.5 compose file runs `postgres:latest`, which is now
version 18 and will not start on the data an earlier version wrote at
`/var/lib/postgresql/data`. Change its image to the version the data is from —
`cat ./aurcache/db/PG_VERSION` says which, `17` for most — and pull only the
server when you upgrade:

```yaml
  aurcache_database:
    image: postgres:17
```

```bash
docker compose pull aurcache
docker compose up -d
```

Moving the data to a newer PostgreSQL is a separate step, with `pg_upgrade` or
a dump and restore.

## On the first start

The log says what it does; nothing needs an answer.

- **The database is converted.** Build flags become `makepkg` flags (`paru`'s
  are dropped), build logs move out of the database into
  `/app/repo/.build_logs`, and the CA the worker protocol uses is created in
  `/app/repo/.ca`.
- **Packages are named after their package base.** 0.5 stored an AUR package
  under the name it was added by; one of a split package's names becomes the
  base it builds from. Where two packages turn out to be one — two names of
  one base, or a package added twice — the one built most recently is kept
  and takes the other's files; each merge is a warning in the log, as is a
  pair it leaves for you to choose between.
- **Dependencies are recorded**, which 0.5 never did: it built a package's AUR
  dependencies inside that package's build. They become packages of their own,
  are built first, and are published to the repository. This runs in the
  background and shows as an operation in progress.
- **Builds 0.5 had queued wait for it** (shown as waiting for dependencies),
  so none starts before what it depends on. One 0.5 was running when it stopped
  is marked interrupted and runs again.
- **Out-of-date packages are rebuilt.** Version checks now queue a rebuild as
  soon as they find a package out of date (`BUILD_ON_NEW_VERSION`), and
  scheduled updates default to daily (`AUTO_UPDATE_SCHEDULE`), where 0.5 left
  both off. A package 0.5 showed as out of date builds within minutes. Set
  `BUILD_ON_NEW_VERSION=false` and `AUTO_UPDATE_SCHEDULE=@never` to keep 0.5's
  behaviour.

## Settings

- **CPU and memory limits, and the number of concurrent builds**, set in the
  0.5 settings page, become the embedded worker's own settings, on its page under
  Workers. Set on one package, they are dropped — a worker's limits are its own —
  and the log names each. Set as `CPU_LIMIT`, `MEMORY_LIMIT` or
  `MAX_CONCURRENT_BUILDS` in the compose file, they apply as before.
- **The builder image** setting is gone; the worker chooses its image,
  `aurcache-builder-v2`. A `BUILDER_IMAGE` in the compose file still applies:
  if it names 0.5's `aurcache-builder`, remove it, as that image is no longer
  updated.
- **The job timeout** is kept, globally and per package.
- **`DB_PWD`** is the password itself. If you percent-encoded it for 0.5 so that
  it differed from `POSTGRES_PASSWORD`, set the plain password.

## Clients and the repository

pacman clients need no change: the repository's URL, name and layout are the
same. The repository directory now also holds `.build_logs` and `.ca`, which the
server never serves. If you serve `./aurcache/repo` with a web server of your
own, or copy it to a mirror, exclude dot directories: `.ca` holds the CA's
private key.

## Other architectures

Packages for `aarch64` and `armv7h` build as they did, through the host's
qemu/binfmt handlers; see [the Docker
worker](../workers/docker.md#other-architectures).

## How the Docker worker builds

This applies to the hybrid image's embedded worker and to any Docker worker,
whether it comes from 0.5 or from 0.6. The Docker worker now runs each build
with a build agent it puts in the build directory. The agent prepares the
container as root, then runs makepkg as an unprivileged user. Packages that
built before still build, but some things change:

- **Build code has no `sudo`.** The container has no capabilities beyond the
  few the agent needs, and it sets `no-new-privileges`. A PKGBUILD that runs
  `sudo` itself now fails. Dependencies are still installed: makepkg asks the
  agent for them.
- **`--install` and `--rmdeps` in a package's build flags have no effect.** The
  container is discarded when the build ends, so there is nothing to install
  into or to clean up, and the worker leaves both flags out.
- **A failure before makepkg is the worker's, not the package's.** If the image
  upgrade (`pacman -Syu`) fails, for example because a mirror is down, the
  server ends the build and queues a new one while the package has attempts
  left. It no longer fails the package. A PGP key that cannot be fetched is
  noted in the build log and the build goes on, as on the chroot worker.
- **Every build starts from a fresh tree.** The persistent build directory
  setting has no effect on this worker.
- **`BUILD_ARTIFACT_DIR` must allow executing.** The agent is run from the
  build directory, so one on a filesystem mounted `noexec` fails every build
  as its container starts. The worker warns about it when it starts.
- **Build containers have a new name**, `aurcache_build_<worker>_<build>`, so
  that two instances on one Docker daemon keep apart. A container an older
  worker left behind under the old name is not removed by the new one.
- **The worker stops its builds when it is stopped.** On `SIGTERM` it removes
  their containers and reports them, and the server queues them again at once.
- **Builds report what they used.** Each build reports its disk use, its VCS
  commits and, where the engine's cgroup allows it, its peak memory. An
  out-of-memory failure says so and names `MEMORY_LIMIT`.
- **`/dev/shm` follows the memory limit.** It is as large as `MEMORY_LIMIT`, or
  2 GiB when no limit is set, where Docker's default is 64 MiB.
- **A custom `BUILDER_IMAGE`** needs pacman, makepkg, gpg and `find`. It no longer needs
  `sudo` or an `ab` user. An image that worked before still works.

The builder image itself has not changed, so a worker that has not been
upgraded yet keeps building with the same image.

## Afterwards

The hybrid image is for keeping an existing setup running. When convenient,
move to a server and a separate worker; see [Migrating](../workers/hybrid.md#migrating).
