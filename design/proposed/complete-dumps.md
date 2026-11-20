# Design: Complete Dumps, and Restoring One into an Empty Database

Status: **Proposed** · Last updated: 2026-11-20

## Motivation

Several ordinary events hand the server an empty database while everything
else about the instance is still there:

- **A PostgreSQL major upgrade.** From 18 the image keeps its data in
  `/var/lib/postgresql/<major>/docker`, so raising the tag finds nothing there
  and initialises a new, empty database beside the old one. Nothing fails; the
  server runs its migrations and starts as a fresh install, while the
  repository, the CA and the build logs on disk still describe the old one.
- **Switching between SQLite and PostgreSQL.**
- **A lost or wrongly mounted database volume**, or a `DB_NAME` pointing at
  another database.

The [lite dump](../implemented/db-export.md) recovers what was configured, but
only that: restoring one re-resolves every package and rebuilds all of them,
and the build history, the structured log and the published files' rows are
gone. That is the right trade for moving a configuration between instances,
and the wrong one for "same instance, new database".

`pg_upgrade` covers only the first case. It needs the old major's binaries
next to the new ones, so it depends on an upgrade image that supports the new
version (`ixsystems/postgres-upgrade` did not yet support 19 when 19 shipped)
and on the PostgreSQL project's packages, which have no armv7 builds. The
server's own data is small, and the server knows its own schema. Copying it
row by row works for any version, any database engine and any architecture,
with no binaries involved.

## Two scopes, one format

A dump gains a scope:

- **Configuration**: today's lite dump, unchanged.
- **Complete**: everything in a configuration dump, plus every table's rows.

A complete dump is a configuration dump with more in it: the lite files are
always written, and `tables/` is added. So everything that takes a dump today
takes a complete one, with today's meaning. Restoring one into a running
instance merges its configuration, with the existing policies (`skip`,
`overwrite`, `merge-patches`, `--clear`). Only one path uses `tables/`:
replaying it into an empty database (below).

```
manifest.json            as today, plus: scope, kind, schema, integrity
packages.json            )
settings.json            ) the lite files, as today
workers.json             )
patches/<pkgbase>.patch  )
tables/<table>.jsonl     one row per line, complete scope only
```

`manifest.json` additionally records, for a complete dump:

- **`schema`**: the names of the migrations applied when the dump was taken.
  This is the dump's schema version. The existing `DUMP_SCHEMA_VERSION` keeps
  versioning the lite files.
- **`tables`**: in insertion order (parents before children, so foreign keys
  hold while loading), each table with its columns and each column's type.
  Restore inserts by these names and types rather than through the entities.
  An entity has every column today's code knows, while the rows were written
  at the dump's schema, as in a migration.
- **`kind`**: `shutdown`, `scheduled`, `pre-migration` or `manual`. See
  [Which dump is current](#which-dump-is-current).
- **`integrity`**: see [Integrity](#integrity).

Rows keep their ids. Lite dumps avoid ids because they are restored into other
instances. A complete dump is replayed only into an empty database, where its
ids are the only ones there, and keeping them is what keeps every reference
between tables intact without remapping.

## Integrity

A dump that was cut short must never be restored, by the startup path or by
any other. Three layers, each sufficient for a different failure:

1. **Atomic appearance.** A dump is written to `<name>.tar.gz.partial`,
   flushed to disk (`fsync` on the file, then on the directory), and only then
   renamed to `<name>.tar.gz`. Nothing lists, offers or restores a `.partial`
   file. One left by a crash is deleted at the next start. A dump that exists
   under its final name was written completely.
2. **Contents verified against the manifest.** The manifest is the archive's
   *last* entry. It lists every other entry with its SHA-256 and size, and
   every table with its row count. A dump without a manifest is refused, and so
   is one where an entry is missing, or where a digest, a size or a row count
   disagrees. This catches what the rename cannot: a dump copied off the server
   and back, or a disk that returned something other than what was written.
   gzip's own trailer catches truncation before the manifest is even reached.
3. **Restore is verify-then-load.** The whole archive is read and checked
   before the first row is inserted, never as rows stream in. Loading then
   runs in one transaction. A restore interrupted halfway rolls back to the
   empty database it started from, so the next start sees the same situation
   and makes the same choice again.

The digests reuse `aurcache-common`'s `digest` feature, which the server
already enables.

## Where and when dumps are written

Stored dumps are a feature an instance turns on, not something it gets
without a configuration change. They live in the directory
`AURCACHE_DUMP_DIR` names, a `server_env` variable with its docs row. It is
meant to be a volume of its own, apart from both the data directory and the
database, so a dump survives losing either one. Without it the server stores
no dumps. Downloading an export and restoring from a file work as before, and
the startup check below still refuses rather than starting empty. The compose
files `setup compose` generates, and the ones shipped in `compose/`, mount
one, so new instances have it from the start. An existing instance adds a
volume and the variable.

Files are `0600`. A stored dump always carries secrets, because without them
it is useless for the exact replay it exists for. That makes the dump volume
as sensitive as the data directory: it holds a copy of the CA key.

- **At clean shutdown** (`shutdown`): after the API and background tasks stop
  and before the database connection closes. This is the dump a PostgreSQL
  upgrade restores from, because `docker compose up -d` stops the old
  container cleanly before it starts the new one.
- **On a schedule** (`scheduled`): daily by default, keeping the most recent
  few. Both are settings, read through `settings::key::*` like any other.
- **Before applying pending migrations** (`pre-migration`).
- **On demand** (`manual`), from the UI or `aurcache-cli dump create`.

A shutdown dump has to finish inside the container's stop timeout, which is
10 s in Docker by default. Measured on a long-running instance (294 packages,
1,184 builds, 3,898 log entries) on 2026-11-20, a JSON export of every table
was 2.5 MB, 270 kB gzipped, and took 0.57 s, including starting one `psql`
per table. The structured log was the largest part, at 1.2 MB of JSON. That
leaves the default timeout ample room, so no `stop_grace_period` is needed. If
the timeout passes anyway, the partial file is all that remains, and rule 1
discards it.

## Which dump is current

A dump is **current** when nothing has changed in the database since it was
taken. Only a current dump is restored without being asked.

The server writes a `RUNNING` marker in the dump directory at start, once the database is
known to be non-empty, and removes it after the shutdown dump is renamed into
place. At the next start:

- **No marker, and the newest dump is a `shutdown` dump**: that dump is current.
- **Marker present**: the previous run ended without a shutdown dump (a crash,
  a kill, a stop timeout). No dump is current, whatever their kinds. The
  newest is older than what that run wrote.

## Restoring at startup

The only exact replay. It runs only when the database is empty, so a dump
never overwrites data, current or not.

1. Connect. If the database has any migration recorded, start normally. The
   rest of this section does not apply.
2. The database is empty. If the data directory holds no instance (no `.ca`,
   no dumps), this is a fresh install: start normally.
3. The data directory holds an instance:
   - **A current dump exists**: verify it, restore it, and log what was
     restored and from which file.
   - **None is current**: refuse to start. The message names the likely causes
     (a PostgreSQL major raised without upgrading, a different `DB_NAME`, a
     lost volume) and lists the dumps on disk with their dates and kinds.
     `AURCACHE_RESTORE=<file>` restores a named one anyway, and
     `AURCACHE_RESTORE=none` starts empty on purpose. Both are
     `server_env` variables, so each gets a row in the environment-variable
     docs.

A restore:

1. runs the migrations up to the dump's `schema`;
2. loads every table in manifest order, in one transaction (see
   [Integrity](#integrity));
3. resets each table's id sequence on PostgreSQL to the largest id loaded.
   This is the one backend-specific statement in the design
   (`setval(pg_get_serial_sequence(...), max(id))`). SQLite needs nothing,
   since `AUTOINCREMENT` follows the rows;
4. runs the remaining migrations, as any start would.

It refuses a dump whose `schema` has a migration this binary does not know:
that dump was taken by a newer server.

A shutdown dump records exactly what the next start would have found.
`ACTIVE` builds, leases, `PUBLISHING` builds and pending repository updates
are then resumed or reaped as after any restart. A dump named with
`AURCACHE_RESTORE` that is not current can disagree with `repo.db` and the
staging directories. Reconciling those is the same recovery startup already
runs (`Repository::sweep` judges by `repo.db`). The restore only adds a
warning that it was older than the state on disk.

## Upgrading PostgreSQL, afterwards

Change the image tag and run `docker compose up -d`. The old container writes
its shutdown dump as it stops. The new one starts on an empty `<major>/docker`,
and the server restores into it. The old data stays in `<old major>/docker`
until the operator deletes it, which is the fallback if anything is wrong.

A replay also writes every table fresh, which `pg_upgrade` does not: that
copies or links the data files as they are. So a replay also drops the space
dead storage still holds, such as a dropped column's values, which stay on
disk until the table is rewritten. On the measured instance, two dropped
`builds` columns held 87 MB of a 101 MB database.
Moving from SQLite to PostgreSQL works the same way: change `DB_TYPE` and
restart.

The `ixsystems/postgres-upgrade` step stays available as an option. Users
running the Arch package against the system PostgreSQL keep following Arch's
procedure, and get these dumps as backups.

## Surface

The Backup section of the settings page becomes a list of the stored dumps:
date, kind, scope, size, and whether the dump is current. Actions:

- **Dump now**, writing a `manual` complete dump to the server.
- **Download** a stored dump, behind the confirmation an export with secrets
  already asks for, since every stored dump has them.
- **Restore configuration…**, which opens the existing restore dialog with
  that dump preselected: dry run, merge policies, `--clear`.
- **Delete**.

Export (download a fresh dump) and Restore from file stay as they are. Export
gains the scope choice. A restore at startup is recorded in the structured
log, so the dashboard and the activity view show that it happened.

API and CLI follow: `GET /api/dumps`, `POST /api/dumps`,
`GET /api/dumps/<name>`, `DELETE /api/dumps/<name>`, `GET /api/dump?scope=`,
and `aurcache-cli dump list|create|get|delete`. The types go in
`aurcache_common::api::dump` beside today's.

The UI does not offer an exact replay. It would replace a database that is not
empty, under a running server, workers holding leases and a repository that
moves on. The startup path exists so that never has to happen.

## Testing

- Round trip: populate a SQLite database, write a complete dump, replay it into
  a fresh one, and compare every table row for row.
- A complete dump committed as a fixture at today's schema, replayed by every
  later version. Later migrations must keep it restoring, and it is never
  rewritten to pass, like the worker protocol fixtures.
- Integrity: a truncated archive, a `.partial` left behind, a flipped byte, a
  missing table, and a row count that disagrees are each refused before
  anything is inserted. A load that fails partway leaves the database empty.
- Currency: the marker after a crash, after a clean stop, and with a newer
  `scheduled` dump than the last `shutdown` one.
- Startup refuses on an empty database with an instance on disk and no current
  dump, and never restores into a non-empty one.

Nothing tests against PostgreSQL today. The sequence reset is the part that
needs it, and it is the argument for a CI job running the backend tests against
a PostgreSQL service.

## Open questions

- Retention defaults: how many `scheduled` dumps to keep, and whether
  `pre-migration` ones expire. At a few hundred kilobytes each, keeping a
  month of daily dumps costs less than one package.
- Whether the structured log ever needs a window rather than all of it. It is
  the part that grows without bound, but on the measured instance it is
  1.2 MB of JSON.
- Whether the existing `db.sqlite.pre-upgrade` copy is still needed once a
  `pre-migration` dump exists.
