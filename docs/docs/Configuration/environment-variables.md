---
sidebar_position: 1
---

# Environment Variables
AURCache can be configured using the following environment variables:

## Settings precedence

Most settings can also be set in the UI — globally on the Settings page or
per-package on the package's Settings page. When the same setting is
configured in multiple places, the resolution order is, from highest to
lowest priority:

1. **Per-package setting** — explicit user intent for one package always
   wins. This lets you override an env-imposed baseline for a single
   package (e.g. give one heavy build more memory) without touching the
   deployment defaults.
2. **Environment variable** — the admin's deploy-time override of the
   global default. Locks the global Settings page tile, but does **not**
   prevent per-package overrides.
3. **Global setting** — UI-set baseline stored in the database.
4. **Static default** — built-in fallback (the "Default" column below).

The UI badges reflect the source: `(default)`, `(inherited)` (= global),
`(inherited from env)`, or no badge when this scope owns the value.

## Database Configuration
| Variable               | Type                  | Description                                                         | Default                   |
|------------------------|-----------------------|---------------------------------------------------------------------|---------------------------|
| DB_TYPE                | (POSTGRESQL\| SQLITE) | Type of Database (SQLite, PostgreSQL)                               | SQLITE                    |
| DB_USER                | String                | POSTGRES Username  (ignored if sqlite)                              | null                      |
| DB_PWD                 | String                | POSTGRES Password  (ignored if sqlite)                              | null                      |
| DB_HOST                | String                | POSTGRES Host   (ignored if sqlite)                                 | null                      |
| DB_NAME                | String                | Database name                                                       | 'db.sqlite' or 'postgres' |

## General Settings

| Variable               | Type          | Description                                                           | Default |
|------------------------|---------------|-----------------------------------------------------------------------|---------|
| VERSION_CHECK_INTERVAL | Duration      | How often to check package versions (`1h`, `30m`, or seconds)         | `1h`    |
| AUTO_UPDATE_SCHEDULE   | String (crontab) | When to rebuild out-of-date packages, as a [schedule](#schedules) (`@never` to disable) | `@daily` |
| BUILD_ON_NEW_VERSION   | Boolean       | Queue a rebuild as soon as a version check finds a package out of date, rather than waiting for the auto-update schedule | `true`  |
| TZ                     | String        | Timezone cron schedules (`AUTO_UPDATE_SCHEDULE`, `MIRROR_RANK_SCHEDULE`) are read in, e.g. `Europe/Paris`. The compose files forward the host's `/etc/localtime` and pass `TZ` through when it is set where compose runs | the host's, else UTC |
| LOG_LEVEL              | String        | Log level                                                             | INFO    |
| LOG_STYLE              | String        | `never` turns off coloured log output, for a log collector that shows the escape codes; anything else keeps it | coloured |
| JOB_TIMEOUT            | Duration      | Longest a build may run (`3h`, seconds). Sent with each job: the worker stops the build at this or its own `WORKER_BUILD_TIMEOUT`, whichever is shorter. Also a setting, which a package can set for itself | `1h`    |
| MAX_ARTIFACT_SIZE      | Size          | Largest package file a worker or a user may upload, e.g. `20G` (a larger user upload is refused with a 413); also a setting, per package or global | `20G` |
| PERSISTENT_BUILDDIR    | Boolean       | Keep a package's build tree between its builds, for one whose tree holds expensive state (a `prepare()` that clones or downloads into `$srcdir`, a very long compile). The tree is kept as is between successful builds: archives are extracted over it, which never deletes, and untracked files survive a VCS update. Turning it off leaves the trees the package kept, one per worker and architecture, unused until each worker's reclaim takes them, oldest first; also a setting, per package or global | `false` |
| DATE_FORMAT            | String        | How the web UI writes dates: field order, zero padding and clock, e.g. `dmy-nopad-12`; also a setting, which a browser can override for itself | `ymd-pad-24` |
| RETIRED_PACKAGE_GRACE  | Duration      | How long a package file stays downloadable after a newer build or a removal takes it out of the repository database (`1d`, `12h`, or seconds), so clients that synced just before can still fetch it | `1d` |
| ACTIVITY_RETENTION     | Duration      | How long an entry stays in the **Logs** page (`90d`, `4w`, or seconds). `0` keeps everything | `90d` |
| PARSE_NETWORK          | Boolean       | Let a PKGBUILD reach the network while the server parses it; also a setting, global | false |
| BUILD_SSH_KEY          | String        | Name of the worker-held SSH key builds use: `default` is the worker's own key, any other a key the worker holds in `WORKER_GIT_SSH_KEYS`. Empty sets nothing: the global setting applies, or `default` when none is stored. A value that is no key name reads as `default`, with a warning in the log. Also a setting, per package or global; see [Build credentials](../workers/credentials.md) | `default` |
| SECRET_KEY             | String        | \>32Byte Random String for singing cookies                            | Random  |
| AURCACHE_REPO_NAME     | String        | Pacman section and database filename prefix; see [Repository name](#repository-name) | `repo` |
| AURCACHE_PUBLIC_URL    | String        | Base URL that pacman clients reach the repository at, and the base of the `Server =` line that `aurcli repo config` gives them. Example: `http://192.168.1.10:8081`. Workers use its scheme, port and path, with the host that they reach the server at, unless their own [`AURCACHE_REPO_URL` or `AURCACHE_REPO_HOST`](../workers/split-chroot.md#connecting-to-aurcache) says otherwise. One that names no port means `AURCACHE_REPO_PORT` | `http://localhost:<AURCACHE_REPO_PORT>` |
| AURCACHE_PROTECTED_DIR | Paths         | Further directories a PKGBUILD parse may not read, colon-separated, added to the built-in list; see [Package parsing](./package-parsing.md) | none |
| AURCACHE_SANDBOX       | Path          | The `aurcache-sandbox` a parse runs under, for an install outside the default place; absolute | `/usr/bin/aurcache-sandbox` |
| AURCACHE_PKGBUILD_BRIDGE | Path        | The `alpm-pkgbuild-bridge` script a parse runs, likewise | `/usr/bin/alpm-pkgbuild-bridge` |
| AURCACHE_HTTP_LISTEN_ADDRESS | IP address | Address the web UI and API listen on; see [Where the server listens](#where-the-server-listens) | `0.0.0.0` |
| AURCACHE_HTTP_PORT     | Integer       | Port the web UI and API listen on                                     | `8080`  |
| AURCACHE_REPO_LISTEN_ADDRESS | IP address | Address the pacman repository listens on                         | `0.0.0.0` |
| AURCACHE_REPO_PORT     | Integer       | Port the pacman repository listens on                                 | `8081`  |

Signing in is configured with `OAUTH_CLIENT_ID`, `OAUTH_CLIENT_SECRET`,
`OAUTH_AUTH_URI`, `OAUTH_TOKEN_URI`, `OAUTH_USERINFO_URI`,
`OAUTH_REDIRECT_URI` and `OAUTH_ALLOWED_USERS`; see
[Authentication](./authentication.md).

### Repository name

Set `AURCACHE_REPO_NAME=custom_name` on the **server** to publish
`custom_name.db` and `custom_name.files` (and their `.tar.gz` archives). Names
must start with an ASCII letter, digit or underscore and contain only ASCII
letters, digits, underscores, dots or hyphens. `options` and `local` are reserved
by pacman.
Invalid names prevent startup.

Workers receive the name when they register; no worker variable is needed.
`aurcli repo config` discovers it from the running server, using a configured
token when authentication is enabled. `--name custom_name` overrides the
detected name; `--port 9000` overrides only the port and still discovers the
name. With `--offline` or an unreachable server, pass `--name custom_name`
(and `--port` if needed) to configure it manually.
Clients must use the matching `[custom_name]` section in `pacman.conf`; the
`Server =` URL stays the same. Distinct names let a client use several AURCache
instances at once.

To rename an existing repository, finish pending publications and stop the
server first. In every architecture directory under `/var/lib/aurcache/repo`,
rename `<old>.db.tar.gz` to `<new>.db.tar.gz` and `<old>.files.tar.gz` to
`<new>.files.tar.gz`. Remove the old `<old>.db` and `<old>.files` symlinks;
startup recreates them under the new name, pointing at the renamed archives.
Then set `AURCACHE_REPO_NAME`, restart the server and workers, and update each
client's `pacman.conf` section. The server refuses startup when it finds only
differently named databases, so changing the variable alone cannot replace a
populated repository with an empty one.

### Where the server listens

The server has three listeners, each with an address and a port: the web UI
and API (`AURCACHE_HTTP_LISTEN_ADDRESS`, `AURCACHE_HTTP_PORT`), the pacman
repository (`AURCACHE_REPO_LISTEN_ADDRESS`, `AURCACHE_REPO_PORT`) and the
worker protocol (`AURCACHE_WORKER_LISTEN_ADDRESS`, `AURCACHE_WORKER_PORT`).
Unset or empty, each takes every IPv4 interface, on 8080, 8081 and 8083.

An address is one IPv4 or IPv6 address, without a port or brackets:
`127.0.0.1` keeps a listener to this machine, `192.168.1.20` to one interface,
and `::` listens on every interface over IPv6 and, on most Linux systems, IPv4
as well. A port is a number from 1 to 65535. A value that is neither stops the
server at startup, naming the variable.

They are for a [native install](../setup/native.md). In a container, every
interface *is* the container's own, and which host addresses and ports reach
it is the port mapping's choice (`127.0.0.1:9080:8080`).

What the server tells others follows the ports it listens on. The `[repo]`
section workers are sent, and the one `aurcli repo config` writes, use
`AURCACHE_REPO_PORT` when `AURCACHE_PUBLIC_URL` is unset or names no port, and
the **Workers** page's copy-and-run command uses `AURCACHE_WORKER_PORT`.
Workers themselves have to be told where the server is: their `AURCACHE_URL`
names the worker port.

Workers verify the worker listener's certificate against the name they dial
it by, which is why `AURCACHE_TLS_SANS` lists them. When
`AURCACHE_WORKER_LISTEN_ADDRESS` names one address, that address is added to
the certificate as well, so a worker can dial it directly. A worker on the
same machine that dials `localhost` needs the listener on a loopback address
or on every interface, and so does the [hybrid image](../workers/hybrid.md)'s
embedded worker.

## Where the server keeps its state

All relative to the working directory, `/app` in the images; see [Docker
Compose setup](../setup/docker.md#what-to-persist) for what to persist.

| Variable                   | Description                                                        | Default |
|----------------------------|--------------------------------------------------------------------|---------|
| AURCACHE_CA_DIR            | The internal CA that signs every worker's certificate, and the repository's signing key when it is signed. Losing it orphans every enrolled worker, and has every client trust a new key | `./data/ca` |
| AURCACHE_SSH_DIR           | The server's SSH key for cloning package sources, and the host keys trusted on first contact; see [SSH package sources](./ssh-sources.md). Losing it means adding a new key to every private repository | `./data/ssh` |
| AURCACHE_BUILD_LOG_PATH    | Build logs, one file per build                                     | `./build_logs` |
| AURCACHE_SOURCE_CACHE_PATH | Checkouts of package sources, reused between builds; safe to lose  | `./source_cache` |
| AURCACHE_MIRRORLIST_DIR    | Mirrorlists, one per architecture; see [Mirrorlist](./mirrorlist.md) | `./repo` |
| OFFICIAL_REPO_CACHE_DIR    | The official repositories' databases, read to resolve dependencies | `<mirrorlist dir>/official_repo_cache` |
| OFFICIAL_MIRRORLIST_PATH   | The mirrorlist those databases are fetched with                    | the x86_64 mirrorlist |

In a container, when `AURCACHE_CA_DIR`, `AURCACHE_SSH_DIR` and
`AURCACHE_BUILD_LOG_PATH` are not set and their defaults are not on a volume,
the server keeps them in the repository, as `repo/.ca`, `repo/.ssh` and
`repo/.build_logs`, which is what a 0.5 compose file persists. Once their
directories are on a volume it moves them there at start; see [Hybrid
image](../workers/hybrid.md#migrating).

## Signing the repository

Off unless `AURCACHE_SIGN` is set; see [Signing the
repository](./pacman-repo.md#signing-the-repository).

| Variable                             | Description                                                        | Default |
|--------------------------------------|--------------------------------------------------------------------|---------|
| AURCACHE_SIGN                        | Sign every package and both databases with the instance's key (`true`/`false`) | `false` |
| AURCACHE_SIGNING_KEY_FILE            | An OpenPGP secret key the operator supplies, armored or binary, which the server reads and never writes | `signing-key.asc` in `AURCACHE_CA_DIR`, created on first use |
| AURCACHE_SIGNING_KEY_PASSPHRASE_FILE | A file holding the passphrase that locks the key; one final newline is ignored | none: the key is not locked |

## Mirrors

What the server fetches packages and official databases from; see
[Mirrorlist](./mirrorlist.md) for how the lists are kept.

| Variable                    | Description                                                        | Default |
|-----------------------------|--------------------------------------------------------------------|---------|
| MIRROR_RANK_SCHEDULE        | When to re-rank the mirrors, as a [schedule](#schedules)           | `@weekly` |
| MIRRORLIST_SERVERS_X86_64   | Mirrors for x86_64 builds, `;`-separated, in place of a ranked list | none |
| MIRRORLIST_SERVERS_AARCH64  | The same, for aarch64                                              | none |
| MIRRORLIST_SERVERS_ARMV7H   | The same, for armv7h                                               | none |
| OFFICIAL_MIRRORLIST_SERVERS | Mirrors for the official databases the server reads itself, `;`-separated, in place of `OFFICIAL_MIRRORLIST_PATH` | none: that mirrorlist is used |
| MIRRORLIST_PATH_X86_64      | Where 0.5.0 read a mounted mirrorlist. A `mirrorlist` there is adopted at every start, unless `BUILD_ARTIFACT_DIR` is set | `./config/pacman_x86_64` |

## Where the server finds the AUR

| Variable     | Description                                    | Default |
|--------------|------------------------------------------------|---------|
| AUR_RPC_URL  | The AUR's RPC endpoint                          | `https://aur.archlinux.org/rpc/v5` |
| AUR_GIT_URL  | Where AUR package sources are cloned from       | `https://aur.archlinux.org` |

Point them at a mirror of the AUR, or at a local copy for an instance that must
not reach the network.

## Cloning package sources over SSH

A package added from a git repository over SSH (`git@host:repo.git`) is cloned
with the server's own SSH key; see [SSH package sources](./ssh-sources.md).

| Variable                 | Description                                                  | Default |
|--------------------------|--------------------------------------------------------------|---------|
| AURCACHE_GIT_SSH_KEY     | A private key to clone with, without a passphrase. When set, no key is generated | a key generated in `AURCACHE_SSH_DIR` |
| AURCACHE_SSH_KNOWN_HOSTS | A `known_hosts` file: only the host keys in it are trusted   | each host trusted on first contact, and held to that key |

Builds run on [build workers](../workers/split-chroot.md), which are configured
on the worker itself rather than here — including how many builds it runs at
once and how long it will let one run.

:::note Per-build container settings
`BUILDER_IMAGE`, `CPU_LIMIT`, `MEMORY_LIMIT` and `BUILD_ARTIFACT_DIR`
configure the per-build container of the
[Docker worker](../workers/docker.md). On the server they do nothing: a worker
limits its own builds with
[`WORKER_BUILD_MEMORY_MAX`, `WORKER_BUILD_CPUS` and their `WORKER_TOTAL_BUILD_*` totals](../workers/split-chroot.md#resource-limits),
and uploads packages over the API rather than through a shared directory.

They are still read by the [hybrid compatibility
image](../workers/hybrid.md), where they
keep their original meanings — `BUILD_ARTIFACT_DIR` is in fact what selects
host build mode there, as it always did.

`MAX_CONCURRENT_BUILDS` is replaced by `WORKER_CONCURRENCY` on each worker (the
hybrid image still reads it, as its embedded worker's default). The value an
upgrade from 0.5 finds for it on its settings page becomes the embedded
worker's; see [Upgrading from 0.5](../overview/upgrading.md#settings).
:::

## Build workers

Workers connect to a dedicated mTLS listener and are configured through their
own environment. See [Build Workers](../workers/index.md).

| Variable                | Type    | Description                                                  | Default |
|-------------------------|---------|--------------------------------------------------------------|---------|
| AURCACHE_WORKER_LISTEN_ADDRESS | IP address | Address the worker protocol listener listens on; see [Where the server listens](#where-the-server-listens) | `0.0.0.0` |
| AURCACHE_WORKER_PORT    | Integer | Port for the worker protocol listener                        | 8083    |
| AURCACHE_TLS_SANS       | String  | Hostnames the worker listener's certificate is valid for, comma-separated; an IP address may be listed too | localhost |
| AURCACHE_WORKER_IMAGE   | String  | Image named in the copy-and-run command the **Workers** page shows; changes nothing about how workers connect | `ghcr.io/lukas-heiligenbrunner/aurcache-worker:latest` |
| AURCACHE_ENROLLMENT_DIR | Path    | A directory shared with workers: one whose certificate request is there is approved without an operator. The bundled compose setup uses it | none |
| AURCACHE_ENROLLMENT_TOKEN | String | A token shared with workers: one that presents it is approved without an operator | none |
| AURCACHE_PREAPPROVED_WORKERS | String | Comma-separated certificate fingerprints approved without an operator; a worker prints its own on first start | none |
| WORKER_SPILL_DELAY      | Duration | How long before worker priority stops holding a job back      | `60s`      |
| WORKER_LIVENESS_TIMEOUT | Duration | How long before a quiet worker stops counting as available    | `60s`      |
| MAX_ATTEMPTS            | Integer | Requeues before a build is failed for good                   | 3       |
| LEASE_TTL               | Duration | How long a build's lease lasts without a heartbeat before it is requeued. Workers are told at registration | `60s` |
| WORKER_HEARTBEAT_INTERVAL | Duration | How often workers send a heartbeat; keep it well inside `LEASE_TTL`. Workers are told at registration | `15s` |
| REAP_INTERVAL           | Duration | How often expired leases are looked for                      | `20s`   |
| REAP_BACKSTOP_GRACE     | Duration | How far past its own timeout a build that still heartbeats is taken back anyway | `5m` |
| WORKER_CERT_VALIDITY_DAYS | Integer | Lifetime of an issued worker certificate                    | 3650    |

None of the three approval paths re-approves a worker you revoked; see
[Approving](../workers/managing.md#approving).

The images set some themselves, which there is no reason to set by hand:
`AURCACHE_CONTAINER_IMAGE` tells the server it runs in a container (and so
which of its directories are on a volume), the hybrid image's entrypoint
sets `AURCACHE_EMBEDDED_WORKER_SECRET` for its own worker, and the server
image points `AURCACHE_SANDBOX` and `AURCACHE_PKGBUILD_BRIDGE` at its copies
under `/usr/local`.

## Schedules

`AUTO_UPDATE_SCHEDULE` and `MIRROR_RANK_SCHEDULE` are written as in crontab:
five fields, `minute hour day-of-month month day-of-week`, read in the server's
timezone (`TZ`). Each field is `*`, a number, a range `1-5`, a step `*/15`, or
a list `1,15`; months and weekdays take names (`jan`, `mon`), and Sunday is 0
or 7. When both day fields are restricted, a day matching either one runs.

`H` stands for a value picked by a hash of the server and the job, so that
servers -- and the AUR and the mirrors they all reach -- are not hit at the
top of the same hour. It is the same value every time:

| Schedule      | Runs                                                  |
|---------------|-------------------------------------------------------|
| `H 3 * * *`   | every day, at a fixed minute past 3                   |
| `H H(1-5) * * *` | every day, at a fixed time between 01:00 and 05:59 |
| `H/15 * * * *`| every 15 minutes, from a fixed offset                 |
| `0 3 * * 1-5` | at 03:00 sharp on weekdays                            |
| `@daily`      | `H H * * *`: every day, at a fixed time               |

The shortcuts are the simplest way to write one, and pick the hashed values
for you: `@hourly` (`H * * * *`), `@daily` (`H H * * *`), `@midnight`
(`H H(0-2) * * *`), `@weekly` (`H H * * H`), `@monthly` (`H H H * *`) and
`@yearly` (`H H H H *`). `@never` turns a job off; an empty value does too.

The settings page shows the next runs as a schedule is typed.

Schedules used to be written with a leading seconds field and Sunday as 1
(`0 0 2 * * 1`). A stored schedule is rewritten on upgrade. One set in the
environment is still read, as the schedule it means, and the activity log
gives it in the new form to write instead.

