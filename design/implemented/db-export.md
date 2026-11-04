# Design: Lite Database Export and Restore

Status: **Implemented** · Last updated: 2026-09-01

Built as described, with two deliberate divergences and one thing this document
got wrong about the schema. Both are recorded inline below where they apply.

## Motivation

An AURCache instance's *interesting* state is small: which packages you asked
for, how you configured them, and which workers you trust. Everything else —
build history, logs, resolved dependency graphs, the packages themselves — is
large, reproducible, and not worth carrying.

Today that small state is only recoverable by copying the whole database volume
plus the CA directory, which makes migrating hosts, taking a backup, or moving
from SQLite to PostgreSQL an all-or-nothing operation.

## Scope: authored versus derived

The boundary is not a table list, it is a question: *did a human write this, or
did AURCache work it out?*

**Exported (authored):**

| Data | Source |
|---|---|
| Package identity and source | `packages.name`, `source_data` |
| Per-package build config | `packages.build_flags`, `platforms` |
| Whether it was asked for directly | `packages.directly_requested` |
| Source patches | `packages.patch` |
| Server config, per-package overrides | `settings` (`pkg_id = -1` vs a real id) |
| Trusted workers and their routing | `workers` |

**Not exported (derived).** `status`, `out_of_date`, `upstream_version`,
`latest_build`, `split_packages`, `provides`, and the `builds`, `files`,
`activities`, `dependencies` and `package_vcs_sources` tables. All of it is
rebuilt by the next resolve or build.

`dependencies` in particular is deliberately excluded: exporting it would risk
importing a graph that disagrees with today's AUR.

## Identity: pkgbase, never IDs

Nothing in the dump refers to a package by row id, including `settings` rows,
which are keyed by pkgbase instead of `pkg_id`.

> **Correction.** An earlier draft said global settings are `pkg_id IS NULL`.
> They are not: the column is `NOT NULL` and globals use a `-1` sentinel,
> because `UNIQUE (pkg_id, key)` depends on it — NULLs do not compare equal, so
> a nullable column would let the same global key be inserted twice. The first
> implementation matched on `None` and silently exported no global settings at
> all; a test caught it.

This removes the id-remapping logic a dump format would otherwise need, and it
keeps the format stable as ids stop appearing in public URIs. A dump is
therefore diffable and mergeable by hand.

## Format

A `.tar.gz`:

```
manifest.json          schema version, AURCache version, secrets flag
packages.json          keyed by pkgbase; no ids, no patch field
settings.json          global and per-package, keyed by pkgbase
workers.json           name, fingerprint, status, arches, affinity, priority
patches/<pkgbase>.patch
```

**A patch is present iff its file is present.** No manifest flag mirrors it,
because a flag and a file can disagree and a file cannot disagree with itself.
Adding or removing a patch is a pure file operation, which also makes a dump
sensible to keep in git.

The manifest's schema version is the load-bearing part: the point of the format
is restoring into a *different* AURCache version, so an import must refuse a
dump newer than it understands.

## Secrets

Off by default. `--include-secrets` adds:

- **API token hashes.** `api_tokens` stores `token_hash`, never the raw token,
  so a dump does not contain anything directly usable to authenticate — but
  restoring it means tokens users already hold keep working.
- **Worker certificates** (`workers.signed_cert`).
- **The CA** (`ca-cert.pem`, `ca-key.pem`), which today lives outside the
  database entirely and is lost by a database-only backup.

The CA and the worker certificates travel **together, as one unit**: a signed
certificate is only meaningful under the CA that signed it, so allowing one
without the other would produce a coherent-looking dump that authenticates
nobody.

This is what makes migration invisible to workers. Without secrets, workers
re-enroll on first contact and are auto-approved because their fingerprints are
already known; with secrets, they reconnect using the certificates they already
hold and their pinned CA fingerprint still matches.

**The CA private key signs worker identities.** Anyone holding a private dump
can mint a certificate the server accepts as a worker — it is by a wide margin
the most dangerous thing in the file. Private dumps are written `0600`, say so
in the manifest, and the web UI asks for explicit confirmation rather than
offering a checkbox that is easy to leave ticked.

Restoring a private dump onto a *second, running* server clones the CA: two
servers able to mint worker certs, with workers unable to distinguish them.
Fine for migration and backup; a footgun for standing up a staging copy.

## Restore

Insert every package row as-is — including dependencies, which carry patches and
settings of their own — then let the existing resolution rebuild the dependency
graph, then enqueue. Replaying the adds instead would need network at import
time and could resolve to a different dependency set than the dump captured,
silently dropping a dependency's patch.

> **Divergence: a third pass, between those two.** Inserting every row before
> resolving anything does remove the question of import order — a dependency
> cycle stops being a special case — but it is not sufficient on its own. A
> local package satisfies a dependency by its pkgbase, by one of its split
> package names, or by something it `provides`, and a dump carries only the
> first: the other two are derived from the PKGBUILD and are not exported. So a
> git-sourced package providing `libfoo` would not be recognised as satisfying a
> dependency on `libfoo`, resolution would fall through to the AUR, and an AUR
> package would be adopted in place of the source the operator chose. The
> split-package case is likelier still.
>
> So every package's source is read *after* the rows are written and *before*
> the graph is rebuilt, filling in `provides` and the split names. Restore has
> to read those sources anyway, since `dependencies` is not exported either.
>
> A second, quieter precondition: imported rows are written `ENQUEUED`, because
> `resolve_local_dependency_resolutions` only considers packages that are
> active, successful or enqueued. A row in any other state is invisible to
> resolution and its dependents fall through to the AUR for the same reason.

> **Divergence: how failure is handled.** "Atomic" holds for the rows and not
> for what follows, because the two cannot be the same. Writing the rows is one
> transaction over local state: all of them or none. Reading sources and
> resolving dependencies is per package and network-bound, and one unparseable
> PKGBUILD does not undo the other 199 — re-running would then be the only way
> back. Such a package keeps its row, since the commonest reason to land there
> is a transient read and the row carries the configuration the operator asked
> for; what it does not keep is silence. The failure is reported per package,
> saying that other packages will not find it and that `--on-existing overwrite`
> is what retries it.

Builds are enqueued automatically. Worker concurrency bounds the result, so a
large restore is a slow trickle rather than a stampede; it is expected to take a
while.

Import is **atomic and previewable**: validate the whole dump, report every
conflict, then apply in one transaction. `--dry-run` prints what would happen.
Otherwise a rejection on package 90 of 200 leaves a half-imported database,
which is worse than either outcome.

### `--clear`

Wipes and replaces whatever the dump carries. A public dump has no CA, so the
CA survives; a private dump replaces it. No separate flag guards this: `--clear`
already says it is destructive, and the rule "the dump replaces what it
contains" then covers packages, settings, workers and the CA uniformly.

### Merge (without `--clear`)

Additive. Three independent policies, because they are three independent kinds
of conflict:

| Conflict | Options |
|---|---|
| Package already exists | `skip` (default) · `overwrite` · `merge-patches` |
| CA and worker certificates | `ignore` (default) · `copy` |
| API token for an existing user | `ignore` (default) · `copy` |

`merge-patches` takes the dump's patch when the target has none, keeps the
target's when the dump has none, and **fails the import** when both have one —
a conflict only the operator can resolve.

The CA defaults to `ignore` in merge mode specifically because replacing it
invalidates every certificate the current workers hold, which is a destructive
act in the mode whose promise is that it only adds.

## Surface

Both directions from the CLI and the web UI.

- `GET /api/dump?include_secrets=` and `aurcache-cli dump [--include-secrets]`
- `POST /api/restore?dry_run=&on_existing=&clear=&secrets=` and
  `aurcache-cli restore <archive> [--dry-run] [--on-existing …] [--clear]
  [--secrets copy]`
- `GET /api/restore/<id>?after=` follows a running import, the same way a bulk
  add is followed: progress is a stored log read by offset, so an observer can
  attach late, and one that disconnects misses nothing.

A dry run that finds a blocking conflict exits non-zero from the CLI, so
`restore --dry-run && restore` cannot walk into the failure the preview existed
to find.

## Open questions

- Should `settings` keys unknown to the importing version be preserved or
  dropped? Preserving them round-trips a downgrade; dropping them keeps the
  table clean.
- Should a dump record which packages were *building* at dump time, so a
  restore can prioritise them?
