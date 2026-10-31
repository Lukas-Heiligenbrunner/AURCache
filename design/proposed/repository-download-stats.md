# Download counts and traffic from the repository's access log

Status: **Proposed** · Last updated: 2026-10-17

AURCache knows everything about building a package and nothing about whether
anyone installs it. Which packages are pulled and how often, how much the
mirror serves, and whether a package nobody has fetched in months is still
worth rebuilding every week are all answered by the web server's access log,
which today goes to `docker logs` and is thrown away -- or, from Rocket's
handler, is never written. This has both write the same log where the server
can read it, and turns it into per-package download counts and repository
traffic, the same whichever of them served the request.

---

## Where things stand

- **Two things serve the repository.** In the compose setups, the `repo`
  service (`nginx:alpine`) publishes the mirror port and serves the
  repository volume read-only with sendfile (`docker/repo-nginx.conf`,
  `compose/docker-compose.yaml`; the one `aurcli setup compose` writes inlines
  the same server block, `aurcache-cli/src/compose.rs:548`). Everywhere else
  (bare metal, `just serve`, the e2e suite) Rocket's `CustomFileServer`
  serves it on 8081 (`aurcache-api/src/custom_file_server.rs:39`, mounted at
  `aurcache-api/src/init.rs:318`).
- **nginx logs to stdout; Rocket logs nothing.** The official nginx image
  links `access.log` to `/dev/stdout`, so each request is a line of
  `combined` format in the container's log, read by nobody. Rocket's handler
  keeps no record of what it served.
- **Files map to packages.** `files (filename, platform, package_id, size)`
  records every published artifact. A retired file leaves `files` and
  `repo.db` at once but stays on disk for `RETIRED_PACKAGE_GRACE`, so clients
  holding an older `repo.db` keep downloading it for a while.
- **Workers download from the repository too.** A build's dependencies come
  from AURCache's own repository, through the pacman configuration the server
  hands the worker (`render_repo_template`, `aurcache-api/src/worker.rs`).
  Those downloads are builds, not users, and nothing marks them.
- **Statistics are about builds.** `GET /stats` (`ListStats`) and
  `GET /stats/dashboard` (`DashboardView`, `aurcache-common/src/api/stats.rs`)
  count builds, packages and repository size.

## What a request means

pacman's traffic has three kinds of request worth telling apart:

| Request | Means | Counted as |
|---|---|---|
| `GET <arch>/repo.db` (and `repo.files`), `200` or `304` | a client ran `pacman -Sy`; `304` when nothing changed since its copy | a **sync** |
| `GET <arch>/<name>-<ver>-<rel>-<arch>.pkg.tar.*`, completed | a client installed or upgraded that package | a **download** |
| everything else: `.sig`, `HEAD`, aborted transfers, `404` | | **traffic** only |

A download is a `200` the server finished sending, or a `206` for an
open-ended range (`bytes=N-`) it finished sending: pacman resumes an
interrupted download from its `.part` file, and the resumed request is the
one that completes it. An aborted transfer counts its bytes, never a
download, so a client retrying three times is one download.

The package and version come from the file name, not from `files`: split
from the right, the last three dash-separated fields are version, release
and architecture, and the rest is the name. That works for a retired file
still on disk, and for a package since deleted.

**Who asked** is one of three kinds, from the user agent:

- `pacman` -- `pacman/<version> ... libalpm/<version>`, the default for
  pacman and every libalpm front-end (paru, yay, pamac).
- `worker` -- a build fetching its dependencies. Workers set
  `HTTP_USER_AGENT` for every pacman they run in a build root (libalpm reads
  it), to `aurcache-worker/<version> (build <id>)`.
- `other` -- curl, browsers, crawlers.

`pacman` is what the numbers are about, and what every view shows unless
asked otherwise. The other two cost one column value to keep, so they are
counted too rather than dropped, and shown on request: a dependency only
builds ever fetch is a fact worth having, and so is a crawler working
through the repository.

## The design

### One log, whoever serves the repository

nginx and Rocket's handler both write the same log, line for line, into the
same directory, and one reader turns that directory into counts. Nothing
after the log knows or cares which server answered: the counts, the API and
the UI are the same with the nginx container or without it, and so are the
raw files left for anyone's own analysis.

The directory is `AURCACHE_REPO_ACCESS_LOGS`
(`settings::key::RepoAccessLogs`, server-wide), by default
`./repo_access_logs` beside `./build_logs`, so a bare-metal or `just serve`
instance counts downloads with no configuration at all. It is never under
the repository root: the log holds client addresses, and the repository is
served to anyone.

Each writer has files of its own, `<writer>-<YYYY-MM-DD>.log`
(`nginx-2026-10-17.log`, `server-2026-10-17.log`): two processes appending
to one file would be safe on a local filesystem and is not on NFS, and
separate names cost nothing. A request is answered by one server or the
other, so nothing is counted twice -- in the compose setups, Rocket's
handler still listens inside the server container but nobody reaches it,
and its file simply stays empty.

### The format

JSON lines, one object per request, the same fields from both writers:

| Field | nginx | Meaning |
|---|---|---|
| `v` | literal `1` | format version |
| `t` | `$time_iso8601` | when the response finished, with its UTC offset |
| `a` | `$remote_addr` | client address, after any trusted proxy header |
| `m` | `$request_method` | |
| `u` | `$uri` | the decoded path, without the query |
| `s` | `$status` | |
| `b` | `$body_bytes_sent` | body bytes actually sent |
| `c` | `$request_completion` | `"OK"` if the whole response was sent, else `""` |
| `r` | `$http_range` | the request's `Range` header, or `""` |
| `ua` | `$http_user_agent` | |

A format of our own (`escape=json`) rather than `combined`: no quoting rules
to get wrong in a user agent, and `combined` has neither the completion nor
the range. `v` lets the format change without guessing which lines are which.
The format is documented in the docs, as the contract for anyone writing it
from another web server or reading it with their own tools.

### nginx

```nginx
# The day the request was logged, from the request's own time.
map $time_iso8601 $log_day {
    "~^(?<d>\d{4}-\d{2}-\d{2})" $d;
}

log_format aurcache escape=json
    '{"v":1,"t":"$time_iso8601","a":"$remote_addr","m":"$request_method",'
    '"u":"$uri","s":$status,"b":$body_bytes_sent,"c":"$request_completion",'
    '"r":"$http_range","ua":"$http_user_agent"}';

server {
    # ... as today ...
    access_log /app/repo_access_logs/nginx-$log_day.log aurcache;
}
```

- **One file per day, named by the variable in the path.** nginx cannot be
  told to rotate from another container, and nobody else would; with the date
  in the name, a new file starts each day by itself, and a finished day is a
  file nobody writes again. nginx opens the file for each line (logs with
  variables in the path are unbuffered), so a line is one `write` and the
  reader never sees half of one.
- **The worker processes create those files**, not the master, so the
  directory must be writable by the `nginx` user. The `repo` service starts
  with `install -d -o nginx -g nginx /app/repo_access_logs` before handing
  over to the image's entrypoint. The generated compose file already runs a
  start command (to write the config); the repository's
  `compose/docker-compose.yaml` gains the same one.
- **A `repo_logs` volume**, mounted read-write at `/app/repo_access_logs` in
  both `repo` and `aurcache`, so the server reads nginx's files where it
  would write its own and needs no setting. Not the repository volume: that
  one is read-only in `repo`, and served.

### Rocket's file server

`CustomFileServer` writes the same line for every request it answers, its
`404`s included:

- **Completion and bytes, as nginx measures them.** The handler builds its
  `200` from the open file itself, as it already does for a `206`, so both
  bodies go through one counting reader. When the reader reaches the end of
  the body, the response is complete (`c: "OK"`); when it is dropped first,
  because the client went away, it is not. Either way the line is written on
  drop, with the bytes read so far. That is nginx's own meaning: "OK" once the
  last byte is handed to the socket.
- **The address** is `Request::client_ip()`, which honours Rocket's
  `ip_header` behind a proxy as `$remote_addr` honours `set_real_ip_from`.
- **The handler does not touch the file.** It sends the line to an
  `AccessLog` handle on `Services`, built like `ActivityLog`: `send` does not
  await and cannot fail, a bounded queue drops when full and counts what it
  dropped, and one task owns the file, appending each line with one `write`
  and opening the next day's file when the date changes. A crash loses the
  lines still queued, a few at most, rather than the counts since a flush.

The server creates the directory `0700` when it is missing, as it does its
other private state: nginx's start step owns it in the compose setups.

### The reader

A task in `aurcache-utils` (`downloads::reader`), spawned by the composition
root, polls the directory every 30 seconds:

1. For each `*-YYYY-MM-DD.log`, in date order, read from its recorded
   offset up to the last complete line.
2. Parse each line into an `AccessEvent` and fold it into the counters
   below, in memory.
3. In **one transaction**, add the counters to their rows and move the
   file's offset in `access_log_cursors (file, offset)`.

Since the offset moves in the same transaction as the counts, a crash or a
restart between the two re-reads exactly what was not recorded: every line
counts once, and nothing is lost while the server is down -- nginx keeps
writing, and the server catches up when it returns.

- A line that does not parse is skipped and counted, and the reader logs the
  first one per file at warning, so a format change shows up once rather than
  as a log full of errors.
- A file shorter than its recorded offset was replaced; the reader starts it
  over and warns.
- A file whose day is over and that is read to the end stays in the
  directory for `REPO_ACCESS_LOG_DAYS` (default 30), then is deleted with its
  cursor. AURCache keeps counts, not every question one could ask of a log;
  the raw files, in a documented format, are there for anyone who wants
  more, with whatever tool they like. `0` never deletes, for a deployment
  that ships them elsewhere and cleans up itself.

Polling, not inotify: the volume may be on network storage, where inotify
says nothing, and a 30-second delay on a statistic costs nothing.

### What is stored

```
download_counts (day, platform, pkgname, version, client, downloads, bytes)
    primary key (day, platform, pkgname, version, client)

repo_traffic    (day, platform, client, requests, bytes, syncs, not_found)
    primary key (day, platform, client)

access_log_cursors (file, offset)
```

- `day` is the request's UTC date, from the time in the line, not from the
  file's name: the writer's local time decides which file a line lands in
  and nothing else.
- `platform` is `Platform`, `client` a `ClientKind` (`pacman`, `worker`,
  `other`), both typed in the entity, as other enum columns are.
- `version` is `[epoch:]pkgver-pkgrel`, as in the file name.
- **Keyed by name, not by `package_id`**, and with no foreign key. A count
  is about a file that was served, which outlives the package row: a
  package deleted and added again keeps its history, and
  `package_delete` leaves these rows alone on purpose.
- `repo_traffic` gets a row for every day the reader ran, downloads or
  none, so "nothing was downloaded" (`0`) and "nothing was recorded" (no
  row, shown as a dash) stay different answers.
- One writer owns all three tables, so adding to a row is a read and an
  insert-or-update in the transaction, through the ORM, with no upsert
  dialect to differ between SQLite and Postgres.

**Size.** At most one row per (day, platform, name, version, client) that
saw a download: a repository of a thousand packages, every one downloaded
every day on two architectures, is well under a million rows a year, and
most packages are not downloaded every day. Rows older than
`DOWNLOAD_STATS_DAILY_DAYS` (default 180) are folded into
`download_counts_monthly`, without the version, by the scheduler.

### Where it shows

| Route | CLI | Gives |
|---|---|---|
| `GET /stats/downloads?days=30` | `aurcli downloads [--days 30] [--client all]` | per day: downloads, syncs, bytes; and the most downloaded packages over the window |
| `GET /package/<pkgbase>/downloads?days=90` | `aurcli downloads <pkgbase> [--days 90]` | per day and per version, summed over the package's split packages |

Both take `client` (`pacman` by default, `worker`, `other`, `all`), and
answer `None` for days before collecting started rather than zeros. Both
need `stats:read` (`api-tokens.md`).

In the UI:

- **Dashboard**: a "Downloads (30 days)" tile and a "Repository traffic"
  tile, beside the build tiles.
- **Package page**: a downloads chart over 90 days, stacked by version, so
  how fast clients move to a new version is visible at a glance.
- **Packages list**: a sortable "Downloads (30 d)" column, from a
  `downloads_30d: Option<u64>` on `SimplePackage`.

## What this does not do

- **Other web servers' logs.** The reader parses its own format. Caddy,
  Traefik or a hand-written nginx config can write the same JSON lines into
  the directory, under a writer name of their own, and nothing else is
  attempted.
- **Importing history.** Logs from before the setting was turned on are not
  read; `combined` does not say whether a transfer completed, so old logs
  would give a different kind of number anyway.
- **Acting on the numbers.** Flagging packages nothing downloads and nothing
  depends on, as candidates to stop rebuilding, is the obvious next use,
  beside `abandoned.rs`. It is a separate change, made once there are
  numbers to look at.
- **Unique clients, and anything else per client.** Counting distinct
  machines means holding addresses, or hashes of them, in the database, and
  still miscounts NAT and roaming laptops. The database holds no address and
  no user agent; the raw files, for as long as they are kept, are where
  such questions go.
- **Mirrors.** Downloads served by a mirror of the repository are that
  mirror's log, not this one.

## Implementation order

1. The migration (`download_counts`, `download_counts_monthly`,
   `repo_traffic`, `access_log_cursors`), the
   `AccessEvent` type and its parser, the file-name splitter, and the fold
   from events to counters, with unit tests for each rule in "What a
   request means".
2. The reader and its cursor, and `CustomFileServer` writing the log
   through `AccessLog`: counts work end to end with no nginx, and the e2e
   suite, which serves with Rocket, covers them.
3. The nginx config, the `repo_logs` volume and start step in both compose
   files and in `aurcli setup compose`.
4. The worker's `HTTP_USER_AGENT` in build roots.
5. The monthly fold and the raw-file cleanup in the scheduler.
6. The routes, the CLI, then the dashboard tiles, the package chart and the
   list column.

## Verification

- Unit: each kind of request in the table above lands where it says; a
  resumed `206` completing a `.part` is one download; an aborted `200` is
  bytes and no download; a `304` on `repo.db` is a sync; a retired file's
  name still parses to its package and version.
- Unit, the two writers agreeing: fixtures of real nginx lines for a full
  `200`, an aborted `200`, a resumed `206`, a `304`, a `404` and a `416`
  (`aurcache-utils/tests/fixtures/access-log/`), and the lines
  `CustomFileServer` writes for the same requests, fold to the same counters.
  A fixture is captured from nginx, never edited to make the test pass.
- Unit: a body dropped halfway writes its line with the bytes sent and no
  completion; one read to the end writes `"OK"`.
- Unit: a malformed line is skipped and counted; a file shorter than its
  cursor is read again from the start.
- Integration, against `sqlite::memory:`: a file read halfway, the
  transaction failed, then read again counts every line exactly once.
- Integration: a consumed file is deleted only after
  `REPO_ACCESS_LOG_DAYS`, never with `0`, and never while it is today's.
- `test-e2e.sh`: after the suite's `pacman -S` from the repository, the
  package's downloads for today are at least one under `pacman`, and the
  worker's dependency fetches are under `worker`, not `pacman`.
- The compose tests in `aurcache-cli/src/compose.rs` check that `repo` and
  `aurcache` both mount `repo_logs` at `/app/repo_access_logs`, the
  server's default, so neither needs the setting.
