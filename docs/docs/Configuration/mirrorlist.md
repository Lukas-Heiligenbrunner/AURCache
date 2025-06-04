---
sidebar_position: 3
---

# Mirrorlist
The mirrorlist is fetched from the archlinux api on initial application load.
With two environment variables the mirrorlist can be auto updaten on a cron schedule and ranked per mirror speed.
Moreover, a mirrorlist can be manually passed, if you want to manage the mirrorlist yourself.

:::info

Only **x86_64** build architecture is supported to set mirrorlist and rerank mirrors at the moment.

:::

## Env Config
| Variable               | Type         | Description                                                                    | Default                   |
|------------------------|--------------|--------------------------------------------------------------------------------|---------------------------|
| MIRROR_RANK_SCHEDULE                | String(CRON) | Auto mirrorlist rank schedule in cronjob syntax with seconds (null to disable) | 0 0 2 * * 0 (once a week) |
| MIRRORLIST_PATH_X86_64                | String       | directory containing mirrorlist inside aurcache container                 | /app/config/pacman_x86_64 |

To enable auto mirror ranking set `MIRROR_RANK_SCHEDULE` to your desired cron schedule and it will automatically rerank the mirrors based on their download speed.

## Manually set mirrorlist
To manually set a mirrorlist mount a directory containing your `mirrorlist` to the same path as `MIRRORLIST_PATH_X86_64` with a volume or bind mount.
(And unset `MIRROR_RANK_SCHEDULE` since it would overwrite your mirrorlist when the cron schedule triggers)
## Example
### Auto Ranking
```ỳaml
services:
  aurcache:
    image: ghcr.io/lukas-heiligenbrunner/aurcache:latest
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
      - MIRROR_RANK_SCHEDULE=0 0 2 * * 0
      ## END HERE
    networks:
      aurcache_network:
    restart: unless-stopped
  aurcache_database:
    image: postgres:latest
    volumes:
      - ./aurcache/db:/var/lib/postgresql/data
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
```ỳaml
services:
  aurcache:
    image: ghcr.io/lukas-heiligenbrunner/aurcache:latest
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
    image: postgres:latest
    volumes:
      - ./aurcache/db:/var/lib/postgresql/data
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

If you use host build mode things get more complicated since your mirrorlist must be accessible from the builder containers.
The default `MIRRORLIST_PATH_X86_64` in host build mode is `BUILD_ARTIFACT_DIR/config/pacman_x86_64`, so just overwrite this directory with your dir containing the mirrorlist or mount another path to this location.
Remember this path has to be within the `BUILD_ARTIFACT_DIR/` directory to be accessible by the builder.
