---
sidebar_position: 4
---

# Upgrading

What changes between releases, and what to do about it. Upgrading across
several releases is each of their sections in turn, oldest first.

## From 0.6 {#from-0-6}

- **A sandbox policy the build user cannot read refuses every build.** On
  0.6.0 a `/etc/aurcache/sandbox-protected` that exists but cannot be read by
  the build user (`0600 root`, say), or that is reached through a symlink to
  nothing, was taken for no policy: builds ran with the paths it lists
  readable. Such a worker now fails every build with `cannot read the
  protected-path list`. Before upgrading a worker, make the file readable by
  everyone (`chmod 644 /etc/aurcache/sandbox-protected`), and fix or remove a
  link to it that leads nowhere. See [the sandbox](../setup/native.md#aurcache-build-the-patched-makechrootpkg).
- **Protecting `/` refuses every build or parse.** On 0.6.0 an entry in
  `/etc/aurcache/sandbox-protected` naming `/`, or a path that resolves to it,
  protected nothing, and nothing said so; so did one in
  `AURCACHE_PROTECTED_DIR` on a native server. Both now refuse: every build,
  every PKGBUILD parse. Name the directories to protect instead.
- **The Docker worker names its build containers and directories after
  itself**: `aurcache_build_<worker>_<build>` and `<worker>_<build>`, where
  `<worker>` is the start of its fingerprint, so that two instances on one
  Docker daemon or one build directory keep apart. A container or directory
  an older worker left under the old name is not removed by the new one.
- **The Docker worker keeps `BUILD_ARTIFACT_DIR` to itself.** It makes the
  directory mode `0700` when it is not there, and warns at startup when it is
  open to others: root in a build container can leave a setuid-root file in
  its build's directory, which anything else that can reach it could run --
  another user on the host, or a PKGBUILD parse in the hybrid image.
  Make an existing one `0700`, owned by the worker's user. The hybrid image no
  longer creates it beforehand.

### How the Docker worker builds

This applies to the hybrid image's embedded worker and to any Docker worker,
whether it comes from 0.5 or from 0.6. The Docker worker now runs each build
with a build agent it puts in the build directory. The agent prepares the
container as root, then runs makepkg as an unprivileged user. Packages that
built before still build, but some things change:

- **Build code has no `sudo`.** The container has no capabilities beyond the
  few the agent needs, and it sets `no-new-privileges`. A PKGBUILD that runs
  `sudo` itself now fails. Dependencies are still installed, by the agent
  before the build, as on the chroot worker.
- **A failure before makepkg is the worker's, not the package's.** If the image
  upgrade (`pacman -Sy`, then `pacman -Su`) fails, for example because a
  mirror is down, the server ends the build and queues a new one while the
  package has attempts left. It no longer fails the package. A dependency
  that cannot be installed still does. A PGP key that
  cannot be fetched is noted in the build log and the build goes on, as on
  the chroot worker.
- **Every build starts from a fresh tree.** The persistent build directory
  setting has no effect on this worker.
- **`BUILD_ARTIFACT_DIR` must allow executing.** The agent is run from the
  build directory, so one on a filesystem mounted `noexec` fails every build
  as its container starts.
- **Builds report what they used.** Each build reports its disk use, its VCS
  commits and, where the engine's cgroup allows it, its peak memory. An
  out-of-memory failure says so and names `MEMORY_LIMIT`.
- **`/dev/shm` follows the memory limit.** It is as large as `MEMORY_LIMIT`, or
  2 GiB when no limit is set, where Docker's default is 64 MiB.
- **A custom `BUILDER_IMAGE`** needs pacman, makepkg, gpg and `find`. It no longer needs
  `sudo` or an `ab` user. An image that worked before still works.
- **Foreign architectures no longer need qemu's `C` flag.** A binfmt handler
  registered for `sudo` to work under emulation can stay as it is.

The builder image itself has not changed, so a worker that has not been
upgraded yet keeps building with the same image.

## From 0.5 {#from-0-5}

An instance set up from the 0.5 docs upgrades by changing the image line and
nothing else: the `aurcache` image is now the [hybrid
image](../workers/hybrid.md), the server and a worker that builds the way 0.5
did. This section is what to do before, and what to expect after.

### Before

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

### On the first start

The log says what it does; nothing needs an answer.

- **The database is converted.** Build flags become `makepkg` flags: `paru`'s
  are dropped, and so are the non-interactive ones every worker now passes
  itself, so 0.5's default leaves none (see
  [Build flags](../Configuration/build-flags.md)). Build logs move out of the
  database into `/app/repo/.build_logs`, and the CA the worker protocol uses
  is created in `/app/repo/.ca`.
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

### Settings

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

### Clients and the repository

pacman clients need no change: the repository's URL, name and layout are the
same. The repository directory now also holds `.build_logs` and `.ca`, which the
server never serves. If you serve `./aurcache/repo` with a web server of your
own, or copy it to a mirror, exclude dot directories: `.ca` holds the CA's
private key.

### Other architectures

Packages for `aarch64` and `armv7h` build as they did, through the host's
qemu/binfmt handlers; see [the Docker
worker](../workers/docker.md#other-architectures).

### Afterwards

The hybrid image is for keeping an existing setup running. When convenient,
move to a server and a separate worker; see [Migrating](../workers/hybrid.md#migrating).
