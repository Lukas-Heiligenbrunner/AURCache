---
sidebar_position: 3
---

# Mirrorlist
The mirrorlist is fetched from the archlinux api on initial application load.
With two environment variables the mirrorlist can be auto updaten on a cron schedule and ranked per mirror speed.
Moreover, a mirrorlist can be manually passed, if you want to manage the mirrorlist yourself.

:::info

A mirrorlist can be set for **any** build architecture with
`MIRRORLIST_SERVERS_<ARCH>`. Automatic **ranking** is x86_64-only: it ranks
official Arch mirrors, and there is no equivalent for Arch Linux ARM. Other
architectures are configured explicitly or left unset — and unset is fine, since
a worker then uses its own image's mirrorlist.

Arch and Arch Linux ARM do not share a URL layout (`$repo/os/$arch` against
`$arch/$repo`), so an ARM mirrorlist cannot be derived from the x86_64 one by
substituting the architecture. It has to be configured separately.

:::

## Env Config
| Variable               | Type         | Description                                                                    | Default                   |
|------------------------|--------------|--------------------------------------------------------------------------------|---------------------------|
| MIRROR_RANK_SCHEDULE                | String (crontab) | When to re-rank the mirrors, as a [schedule](environment-variables.md#schedules) | `@weekly` (once a week, on a day and at a time picked for this server) |
| MIRRORLIST_PATH_X86_64                | String       | directory containing mirrorlist inside aurcache container                 | /app/config/pacman_x86_64 |
| MIRRORLIST_SERVERS_X86_64                | String       | semicolon-separated list of mirror URLs (disables auto ranking)                 | null |
| MIRRORLIST_SERVERS_AARCH64                | String       | the same, for aarch64 workers (no auto ranking for this arch)                 | null |
| MIRRORLIST_SERVERS_ARMV7H                | String       | the same, for armv7h workers                 | null |
| OFFICIAL_MIRRORLIST_SERVERS                | String       | mirrors used for the server's **own** official-repo database lookups           | the x86_64 list |

## Manually set mirrorlist via env var

Use `MIRRORLIST_SERVERS_X86_64` with semicolon-separated mirror URLs:

```yaml
services:
  aurcache:
    image: ghcr.io/lukas-heiligenbrunner/aurcache-server:latest
    environment:
      - MIRRORLIST_SERVERS_X86_64=https://mirror.rackspace.com/archlinux/$$repo/os/$$arch;https://mirrors.kernel.org/archlinux/$$repo/os/$$arch
    # ... rest of config
```

When this env var is set, automatic mirror ranking is disabled.

## Manually set mirrorlist via file mount

To enable auto mirror ranking set `MIRROR_RANK_SCHEDULE` to your desired [schedule](environment-variables.md#schedules) and it will automatically rerank the mirrors based on their download speed.

## Manually set mirrorlist
To manually set a mirrorlist mount a directory containing your `mirrorlist` to the same path as `MIRRORLIST_PATH_X86_64` with a volume or bind mount.
Ranking stands down on its own while a mirrorlist is mounted, rather than overwrite it. To turn it off anyway, set `MIRROR_RANK_SCHEDULE=@never`.
## Example
### Auto Ranking
```ỳaml
services:
  aurcache:
    image: ghcr.io/lukas-heiligenbrunner/aurcache-server:latest
    ports:
      - "8080:8080" # Frontend
      - "8081:8081" # Repository
    volumes:
      - ./aurcache/repo:/app/repo
    privileged: true
    environment:
      - DB_TYPE=POSTGRESQL
      - DB_USER=aurcache
      - DB_PWD=YOUR_SECURE_PWD
      - DB_HOST=dbhost
      ## HERE
      - MIRROR_RANK_SCHEDULE=@weekly
      ## END HERE
    networks:
      aurcache_network:
    restart: unless-stopped
  aurcache_database:
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

networks:
  aurcache_network:
    driver: bridge
```

### Manually set Mirrorlist
```yaml
services:
  aurcache:
    image: ghcr.io/lukas-heiligenbrunner/aurcache-server:latest
    ports:
      - "8080:8080" # Frontend
      - "8081:8081" # Repository
    volumes:
      - ./aurcache/repo:/app/repo
      - ./hostmirrorlistpath:/app/config/pacman_x86_64 # the container path must match with MIRRORLIST_PATH_X86_64
      # hostmirrorlistpath is a directory containing a `mirrorlist` file with your mirrorlist
    privileged: true
    environment:
      - DB_TYPE=POSTGRESQL
      - DB_USER=aurcache
      - DB_PWD=YOUR_SECURE_PWD
      - DB_HOST=dbhost
    networks:
      aurcache_network:
    restart: unless-stopped
  aurcache_database:
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

networks:
  aurcache_network:
    driver: bridge
```

## Where the mirrorlist is used

A mirrorlist set here applies to every worker, including remote and
foreign-architecture ones: the server sends the one matching the build's
architecture with each job. Workers need no configuration for this to work.

The server also fetches the official repository **databases** (`core`, `extra`,
`multilib`) to decide whether a dependency already exists in the official repos
and so needs no AUR build. That fetch is small and hourly, and by default uses
the same x86_64 mirrors. Set `OFFICIAL_MIRRORLIST_SERVERS` to point it
elsewhere — useful when the server should read its index from a mirror close to
*it* while workers download packages from mirrors close to *them*.

## Overriding the mirrorlist on a worker

A worker on other hardware, or on the far side of a slow link from the mirror
the server prefers, can use its own mirrors instead. Set either variable on the
**worker**:

| Variable | Type | Description |
|---|---|---|
| WORKER_MIRRORLIST_SERVERS | String | semicolon-separated mirror URLs, used in place of the server's |
| WORKER_MIRRORLIST_FILE | String | path to a ready mirrorlist file inside the worker container |

`WORKER_MIRRORLIST_SERVERS` wins if both are set. A worker configured this way
tells the server not to send one at all.

Resolution order for each build:

1. the worker's own mirrorlist, if configured
2. the server's mirrorlist for that architecture, if it has one
3. the worker image's own `/etc/pacman.d/mirrorlist`

Nothing is required at either end: with no configuration anywhere, the server
ranks x86_64 mirrors for itself and foreign-architecture workers use their
image's defaults.

:::note Bandwidth
The mirrorlist travels with a build job, but only when it has actually changed.
Each job carries a checksum of the list the worker already holds, and the server
resends the content only on a mismatch — so a rerank reaches every worker on its
next build without shipping the same bytes with every job.
:::

:::note Hybrid image
In the [hybrid compatibility
image](../workers/hybrid.md), whose
legacy container builder spawns a container per build, the mirrorlist must live where the spawned build
containers can reach it — the default `MIRRORLIST_PATH_X86_64` is then
`BUILD_ARTIFACT_DIR/config/pacman_x86_64`, and any path you mount instead has to
be inside `BUILD_ARTIFACT_DIR/`.
:::
