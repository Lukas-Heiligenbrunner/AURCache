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
token and no approval click: sharing the `enroll` volume is what tells the
server this worker is trusted. For more build throughput, raise the worker's
concurrency on its page in the web UI (or `WORKER_CONCURRENCY_DEFAULT`): each
build already uses every core, so concurrency buys builds at once, not faster
builds. For a worker on another machine, where there is no shared volume, see
[Build Workers](../workers/split-chroot.md).

## With PostgreSQL

SQLite is fine for trying AURCache out, but PostgreSQL is recommended for
anything you intend to keep. [`aurcli`](../cli/index.md#installing) writes the
same file with a database service beside the server:

```bash
aurcli setup compose --database postgres
docker compose up -d
```

The database is `postgres:18-trixie`, with a generated password and a
healthcheck the server waits on before it migrates. [Setup and
health](../cli/setup.md#a-compose-file-instead) covers the options, including
the step that upgrades the data when you raise PostgreSQL's major version.

Keep the Postgres image pinned as generated rather than `postgres:latest` or even
`postgres:18`. Those tags move to a newer Debian release from time to time, and
a database whose indexes were sorted by the old C library then needs its
indexes rebuilt. AURCache checks for that at startup and logs the commands to
run if it finds one (`REINDEX DATABASE` then `ALTER DATABASE ... REFRESH
COLLATION VERSION`, in each database the warning names).

## Or let the CLI do it

`aurcli` can also stand the pair up without a compose file, with `docker run`,
and needs no token or running server to do it:

```bash
aurcli setup server      # the backend, on this machine
aurcli setup worker      # one build worker beside it
aurcli doctor            # check they found each other
```

[Setup and health](../cli/setup.md) covers more workers, workers on other
hardware, their storage pool, and compose files for TrueNAS, Portainer or
Unraid.

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
aurcli repo config --install           # asks for sudo only if it has to
sudo pacman -Sy
```

If a build does not start, [`aurcli doctor`](../workers/managing.md#why-is-a-build-not-starting)
says why.

`aurcli completions <shell>` prints a completion script for bash, zsh,
fish, elvish or powershell.

## Upgrading from a single-container setup

If you already run AURCache as one container, it keeps working: change the
image line and nothing else. [Upgrading from 0.5](./upgrading.md) says what to
back up first and what the first start does; the [hybrid compatibility
image](../workers/hybrid.md) is what it then runs.

For more advanced setup see the
[Configuration](/docs/Configuration/environment-variables) page.
