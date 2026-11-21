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

## One package per package name

A package name in the repository is published by one package at a time. A
build that would publish a name another package publishes on that platform
fails, and its log names that package -- whichever kind of package either one
is, and whatever versions their files are. Earlier releases published both
whenever their versions differed, so `repo.db` could list the name twice.

Packages that already share a name when you upgrade keep the files they
have in the repository, but no build of either that publishes the name
succeeds any more: each is refused because the other publishes it. The log
names every such name at each start (`'x' is published on x86_64 by more than
one package`). Remove all but one of those packages; the one that is left then
builds again.

A dependency may take a name over from a package that depends on it once that
package's own source no longer builds it -- a split package moved from one to
the other -- and the dependent's file of it then leaves the repository.
Which names that package's source builds is read from the split packages
recorded for it: for one whose sources were never read, only its own name
counts, so a dependency can take over its other split names.

## Going back to an earlier release

Restoring the backup taken before upgrading is the way back from any release.
Running an earlier server on the database a later one left is not supported,
and some changes make it fail outright:

- **Uploaded builds.** Once a package has been
  [uploaded](../cli/packages.md#uploading-packages-built-elsewhere), its
  build row records that it was uploaded, which a server from before uploads
  cannot read: anything that reads that build fails there.
