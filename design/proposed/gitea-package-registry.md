# Publishing to a Gitea (or Forgejo) Arch package registry

Status: **Proposed** · Last updated: 2026-10-17

Gitea and Forgejo both host Arch package registries: pacman installs from
them, they build and sign the package database themselves, and access follows
the forge's users, organizations and tokens. Some operators already run one
and would rather their clients have a single place to install from. This doc
asks whether AURCache should publish to such a registry **in addition to** its
own repository, or **instead of** it, and how.

Short answer: in addition to it, through a reconciler that keeps the registry
in step with what `repo.db` lists. "Instead of" is offered as a choice about
which repository clients are told to use, not as removing the internal one,
because the build pipeline depends on the internal one.

---

## Where things stand

- The repository is one directory per platform (`x86_64`, `aarch64`,
  `armv7h`; `pacman-mirrors/src/platforms.rs:7`) with `repo.db`/`repo.files`
  and the package files. It is named `repo` (`aurcache-common/src/repo.rs:18`)
  and served unauthenticated over HTTP.
- Every change goes through `Repository` (`aurcache-utils/src/repository.rs:49`):
  under its lock, the new databases are prepared beside the old ones, the SQL
  changes and a recovery plan commit together, then files and databases are
  renamed into place (`commit`, `:292`). A retired file leaves `repo.db` at
  once and stays on disk for `RETIRED_PACKAGE_GRACE`
  (`aurcache-scheduler/src/retired_packages.rs:26`) before `sweep` (`:574`)
  deletes it.
- **Workers install from this repository.** The worker template gives each
  chroot a `[repo]` section with `SigLevel = Never`
  (`aurcache-api/src/worker.rs:88`), appended to the job's `pacman.conf`
  (`aurcache-utils/src/job_config.rs:68`). A build whose dependency is another
  AURCache package gets it from here, and dependents are promoted as soon as
  the dependency is published (`worker_complete::trigger_dependents`). The
  internal repository is part of the build pipeline, not only its output.
- Nothing is signed (`design/proposed/signed-repository.md`).
- `plugins.md` lists "repository mirroring" (S3, rsync, CDN purge) as plugin
  example 13, on the `build.published` and `repo.swept` events. A Gitea
  registry is a mirror of that kind, with one difference covered below: the
  target builds its own index, so it is fed packages, not a directory tree.

---

## What the registries offer

Checked against Gitea's documentation and its `routers/api/packages/arch`
and `services/packages/arch` sources, and against Forgejo's documentation.

| | Gitea | Forgejo |
|---|---|---|
| Upload | `PUT /api/packages/{owner}/arch/{repository}`, the raw package as body | `PUT /api/packages/{owner}/arch/{group}`, same body; `.pkg.tar.zst` only |
| Delete | `DELETE /api/packages/{owner}/arch/{repository}/{name}/{version}/{arch}` | same shape, with `{group}` |
| Auth | HTTP Basic, user and token (`write:package`) | same |
| Duplicate | `409` when `name-version-arch.pkg.tar.*` already exists in that repository and arch; no overwrite, delete first | not documented; assume the same |
| Index | built eagerly on each upload and delete, one `packages.db` per repository and arch, with `%FILES%` inside it; `.db` and `.files` both serve it | built by the server; details not documented |
| Versions listed | **only the newest version of each package**; older versions stay stored and downloadable but drop out of the index | not documented |
| `any` | one `any` slot per repository, merged into every arch's index; a download falls back to `any` | not documented |
| Signing | the server signs **every package and every index** with a key per owner, created on first use; an uploaded `.sig` is not accepted | the server signs |
| Client key | `GET /api/packages/{owner}/arch/repository.key` | same |
| Client stanza | `[{repository}]`, `Server = …/api/packages/{owner}/arch/$repo/$arch` | `[{owner}.{group}.{domain}]`, `Server = …/arch/{group}/$arch` |
| Quotas | `403` when the owner's package quota is exceeded | — |

The two implementations come from different pull requests (Gitea's, and
Forgejo's PR 4785, which started as Gitea PR 31037). They agree on the upload
and delete shapes, which are all a publisher needs, and differ in what
clients write. The behaviour Forgejo leaves undocumented has to be confirmed
by a spike (see [Open questions](#open-questions)).

What follows for AURCache:

- **The registry has no transaction.** Every upload or delete rebuilds that
  arch's index on its own. A split package's outputs, or a version bump
  across a dependency chain, appear one file at a time.
- **Newest-wins indexing helps with replacement.** Uploading `foo-2.0`
  supersedes `foo-1.0` in the index without touching `foo-1.0`'s file, so a
  client holding the older index can still download `foo-1.0`. That matches
  `Repository`'s grace period without any extra work.
- **The same filename cannot be uploaded twice.** A rebuild at the same
  `pkgver-pkgrel` (a manual rebuild; ABI rebuilds bump `pkgrel`, see
  `abi-rebuilds.md`) needs a delete followed by an upload.
- **A downgrade cannot be published by upload alone.** If the registry holds
  `foo-2.0` and AURCache now lists `foo-1.9`, `foo-2.0` keeps winning until it
  is deleted.
- **The registry signs.** Clients can run `SigLevel = Required` against it
  today, whatever AURCache does about signing.

---

## In addition, or instead of

### In addition: a mirror

The internal repository stays exactly as it is, and the registry follows it.
Publishing, dependent promotion and the workers' chroots are unaffected, and
when the registry is down or misconfigured, builds carry on and the registry
catches up later. This is the proposal.

### Instead of: what it would take

Removing the internal repository would mean:

- **Workers install AURCache-built dependencies from the registry.** A
  dependent is promoted once its dependency is published, so promotion would
  have to wait until the registry has accepted the upload, and a registry
  outage would stop every chained build. Workers would need registry
  credentials when the owner is private, and every one of them would have to
  trust the registry's key (or keep `SigLevel = Never`).
- **Publishing loses its guarantees.** `Repository` exists so that a commit
  either happens completely or not at all, can be recovered after a crash, and
  never loses a concurrent change. The registry has none of that: a split
  package is half-published between two uploads, and a same-version rebuild
  disappears from the index between its delete and its upload. The `files`
  table, the recovery plan and the sweep would all need counterparts that the
  registry cannot provide.
- **Every place that reads the repository changes**: downloads in the UI,
  the sweep, `aurcache-cli repo config`, and `/repo/info`.

That is a second publishing backend, weaker than the first, for a gain that
the mirror mostly delivers. So "instead of" is offered as **presentation**:

- A setting picks which repository AURCache **advertises**: `internal` (the
  default) or the registry. With the registry, `repo config`, the UI's
  repository page and `/repo/info` give the registry's stanza and key, and
  the internal repository is only what workers use.
- An operator who also wants the internal repository unreachable from
  outside closes it at the proxy or firewall. The workers reach it by a host
  each of them already knows (`worker.rs:80`), so nothing in AURCache needs
  to change for that. Restricting the file server to workers inside AURCache
  (it is plain HTTP, and workers have mTLS identities only on the API) is a
  separate change, and not proposed here.

What would reopen the full "instead of": a need for the internal repository to
disappear from the server's disk, for example a server without storage for
artifacts. Promotion gated on the mirror (below) is the piece that would make
it possible.

---

## Proposal: a reconciler

One background task per configured registry (one registry in the first cut)
compares what the registry **should** list with what AURCache has **put
there**, and makes the difference.

### Desired state: `repo.db`

The desired state for a platform is the set of entries in that platform's
`repo.db` — filename, name, version and arch. It is read without the
repository lock, since `repo.db` is replaced only by an atomic rename, the
same reasoning the sweep relies on (`repository.rs:563`). Reading `repo.db`
rather than the `files` table means that the registry follows what clients of
the internal repository actually see. A publish that is still `PUBLISHING`,
or one whose finalization has failed, is not mirrored until it lands.

### Recorded state: what AURCache uploaded

A table `mirror_uploads`, owned by the reconciler task and written only by it
(the same rule as other background state, so no call site writes it):

| column | |
|---|---|
| `target` | which registry (one, for now; a key so that a second costs nothing) |
| `platform`, `filename` | as in `repo.db` |
| `name`, `version`, `arch` | for the delete URL and for version comparisons |
| `pkgbase` | the AURCache package it came from, for step 3 below |
| `sha256` | of the file uploaded |
| `uploaded_at` | |
| `superseded_at` | when the **registry** stopped listing it: when the upload of a newer version of the same name succeeded. `NULL` while it is still the newest there |

The reconciler **only deletes what it recorded uploading**. Listing the
owner's packages through Gitea's API cannot tell which `{repository}` a file
belongs to, and an operator may publish other things under the same owner.
Recording uploads is what makes deletion safe. It is the same principle as
the sweep judging by `repo.db` and not by the `files` table: a mistake on the
recorded side must not remove things that are not ours. The migration reads
and writes explicit columns, as every migration after
`m20261008_000000_from_0_5` does.

### The pass

For each platform, the reconciler plans a list of actions from desired and
recorded state with a pure function, and then carries them out one at a time,
recording each one as it succeeds:

1. **Upload** each listed file that has no record, or whose record has a
   different `sha256`. It streams the file from the platform directory.
   - `201`: record it.
   - `409` with a recorded file of the same name: this is a same-version
     rebuild, so delete it and upload again. The package is missing from the
     index for the length of one request.
   - `409` with no record: the file was put there by something else, or the
     record was lost. Fetch the registry's checksum for that file (package
     files API). If it matches, record it. If it does not, report the conflict
     and leave the file alone, because it is not ours to delete.
2. **Delete at once** any recorded version that would **shadow** a listed
   one, that is, the same name with a higher version (`vercmp`). Without that,
   the registry would keep offering a version AURCache has rolled back from.
3. **Delete at once** any recorded file whose `pkgbase` is no longer an
   AURCache package, or is opted out of the mirror. Nothing would supersede
   it, so it would otherwise stay installable from the registry. It is
   judged by the `packages` table, **not** by absence from `repo.db`. A
   package that exists but has nothing listed (a fresh restore whose
   rebuilds have not landed yet, a platform directory lost and being
   rebuilt) keeps its registry files until a build of it is listed. Judging
   by `repo.db` would make an empty repository mean "delete everything on
   the registry".
4. **Delete after the grace period** recorded files that a newer upload has
   superseded **on the registry**: `superseded_at` is set when that upload
   succeeds, and the file is deleted once `RETIRED_PACKAGE_GRACE` has passed,
   the same grace period the sweep uses. Until then it is out of the
   registry's index but still downloadable, which is what clients holding an
   older index need. The clock starts at the upload and not when `repo.db`
   stopped listing the file. Otherwise, after a registry outage longer than
   the grace period, the old file would be deleted the moment its
   replacement landed, under a client that had synced just before.

Order inside one pass: uploads in dependency order, dependencies first (from
the `dependencies` table), and each pkgbase's outputs back to back, before
any deletes. Uploading a dependent's new version before the dependency it
now requires would leave registry clients briefly unable to resolve it. The
split-package window, in which a pkgbase's outputs disagree on version, is
as short as the uploads themselves. Neither window can be closed, because
the registry has no transaction.

The pass is idempotent, and a crash between an action and its record repeats
that action at worst. A repeated upload gives `409` with a matching checksum,
and a repeated delete gives `404`; both are treated as done.

### When it runs

- After every finalized repository update: `Repository` exposes a
  `watch::Receiver<u64>` that ticks once its renames are done, and the
  reconciler wakes on it. A missed tick costs nothing, because the next pass
  reads `repo.db` again.
- On a timer (hourly), to retry failures and to carry out step 4 once the
  grace period has run out.
- At startup, and when the configuration changes.
- With exponential backoff on network errors and `5xx`. A `401`, `403` or
  quota error stops the reconciler until the configuration changes, because
  retrying would not fix it.

Enabling it on an existing instance uploads everything listed, one file at a
time. With hundreds of packages that takes a while, and the status shows how
many files are left.

### When the registry is unavailable

Publishing to the internal repository **never waits for the registry**.
Builds publish, dependents are promoted, and workers install from the
internal repository as usual. The registry falls behind, and its clients see
the versions it had when it went away. They are older, but they are
consistent with each other and still signed.

There is no "sync back" to do when it returns, and no queue of missed
changes to replay. The reconciler never acts on events, only on the
difference between `repo.db` and its records, so the first pass after the
outage is an ordinary pass with a larger difference:

- **Only the current state is sent.** If `foo` went 1.1 → 1.2 → 1.3 during
  the outage, the registry goes straight from 1.1 to 1.3. Nothing is lost,
  since the registry's index would only ever have listed the newest
  version.
- **Nothing has to be kept on disk for the registry.** The internal sweep
  deletes files retired during the outage as usual. The reconciler only ever
  uploads what `repo.db` currently lists, and that is always on disk.
- **The grace period still holds** for the registry's own clients, because
  it counts from the superseding upload (step 4).
- **Removals made during the outage** are applied at once (step 3), so a
  package deleted from AURCache during the outage stays installable from the
  registry until the outage ends. That is the one thing an outage leaves
  visibly wrong on the registry, and nothing short of gating deletion on the
  registry would avoid it. Not proposed.
- **Order** follows the dependency order above, which matters most here,
  since a long outage piles up chains of bumps.

The direction is always internal → registry. The registry is never a
source: nothing AURCache needs lives only there, and the reconciler never
reads a package back from it.

**What the operator sees**: the status's `pending` count and the age of the
oldest pending change. `mirror.lagging` (a warning) is raised once a change
has been pending longer than a threshold (an hour by default), and cleared by
the pass that catches up. `doctor` reports the same lag. An outage is
unremarkable for the first few minutes and loud after that.

**Gating, as an option.** An operator who wants the two repositories never
to differ could ask for publishing to wait for the registry. That is the
same "promotion gated on the mirror" mechanism a true "instead of" would
need, and it turns every registry outage into a stop for every build chain.
It stays in the open questions and is not proposed.

### When the registry changes behind our back

The records are a belief about the registry, and they can go stale: someone
deletes a version by hand in Gitea, Gitea is restored from an older backup,
or an admin clears the owner's packages. A pass compares `repo.db` with the
records and would never notice.

So once a day (and on demand from the UI or CLI) an **audit** pass lists the
registry's files for the configured owner and checks each record against
them. A record whose file is missing is dropped, and the next pass uploads
it again. A file whose checksum differs from the record is reported as
`mirror.conflict` and left alone. A file that is present but unrecorded is
left alone too, as everywhere else: not ours. The audit is the only pass
that lists the registry. Ordinary passes trust the records, which keeps them
to one request per change.

### `any`, debug and compression

- **`any`**: AURCache publishes an `any` package into each platform it was
  built for. The registry has one `any` slot per repository, so the
  reconciler uploads it once and records it against each platform that lists
  it. A second platform whose build of the same version differs byte for byte
  (builds are not reproducible) gets `409`, which is "already there" for
  `any`, not a conflict.
- **Debug packages** are not built (`OPTIONS+=(!debug)`,
  `job_config.rs:43`), and they would be uploaded like any other file
  anyway.
- **Compression**: Forgejo accepts only `.pkg.tar.zst`. A package whose
  `PKGEXT` is something else is reported as unmirrorable for Forgejo rather
  than failing the pass.

---

## Signing and trust

The registry signs every package and its index with its own per-owner key.
For clients of the registry, that settles `SigLevel = Required` without
`signed-repository.md`, and it also places the trust in the registry: a
compromised forge can sign anything. That is the server-side model which
`signed-repository.md` already accepts for AURCache's own key, with the
forge as the server.

- If AURCache's own signing lands, its `.sig` files are **not** carried
  over: neither registry accepts a detached signature, and the registry's
  re-signing replaces it. The provenance that a worker-side key would give
  (`signed-repository.md`, "Who signs") does not survive the mirror. That is
  a limitation of the registries, and worth saying in the docs.
- The upload is the trust boundary. The token's owner can publish anything
  under that owner, so the token deserves the same care as the worker CA key.
  A dedicated organization, and a bot user that belongs to it alone, keep the
  damage of a leaked token to that organization.
- AURCache verifies each upload: it checks the `sha256` the registry reports
  for the uploaded file against its own, so that a proxy which mangled the
  body is caught.

---

## Configuration

| setting | scope | |
|---|---|---|
| `mirror.gitea.url` | global | base URL of the forge |
| `mirror.gitea.flavor` | global | `gitea` or `forgejo`: only changes the client stanza |
| `mirror.gitea.owner` | global | user or organization |
| `mirror.gitea.repository` | global | `{repository}` / `{group}`; default `aurcache` |
| `mirror.gitea.user` | global | the Basic-auth user |
| token | a file, see below | never a setting |
| `mirror.gitea.enabled` | **package** and global | a package can opt out; global turns the mirror off without losing the configuration |
| `repository.advertised` | global | `internal` or `gitea` (see [Instead of](#instead-of-what-it-would-take)) |

The settings go through `settings::get` with typed keys, like every other
setting. Opting a package out makes the reconciler treat its files as not
listed for that registry, so turning the setting off removes the package
from the registry by step 3.

### The token

The token stays **out of the database**. The settings table is in every
dump, including the default one, and in every raw database backup. A token
that leaks from there can publish packages under the owner's name. It is
handled the way the worker CA's key is:

- **Where it lives**: `mirror-credentials.json` (mode 0600) in the directory
  that holds `ca-key.pem` (`aurcache-ca/src/lib.rs:20`), keyed by target, so
  a second registry later is a second entry.
- **How it gets there**: the operator sets it from the UI or with
  `aurcache-cli mirror set-token`, through an admin-only
  `PUT /api/mirror/credentials`. It is write-only: no endpoint returns it,
  and the UI shows only whether one is set and when. The server writes the
  file atomically (a temporary file, then a rename) and wakes the reconciler.
  The `setting.changed` event records that it changed, never its value.
- **An external file**: `AURCACHE_MIRROR_GITEA_TOKEN_FILE` points at a
  Docker secret or another mount instead. The server then only reads it,
  never writes it, and setting the token from the UI is refused with a
  message naming the variable. There is no plain environment variable for
  the token itself: it would show up in `docker inspect` and in
  `/proc/*/environ`.

---

## Dump and restore

A private dump (`--include-secrets`) followed by a full restore (`--clear`)
has to bring back a working mirror: the same registry, the same token, and
the same knowledge of what this instance put there. A default dump has to
bring back everything except the token, and the mirror has to wait safely
until the token is set. Three things travel, in three different ways.

### The configuration: already covered

URL, flavor, owner, repository, user, `enabled` and `repository.advertised`
are ordinary settings, global or per-package, so `settings.json` carries them
in every dump (`aurcache-utils/src/dump.rs:140`). Nothing to add.

### The token: a secret, like the CA

`DumpSecrets` (`aurcache-common/src/api/dump.rs:147`) gains
`mirror_credentials: Vec<DumpMirrorCredential>` (target, user, token),
written as `mirror-credentials.json` next to `ca-key.pem` and `tokens.json`.

- **Dump**: `build_secrets` (`dump.rs:109`) reads the token the server uses,
  from wherever it comes: the managed file, or the external file when
  `AURCACHE_MIRROR_GITEA_TOKEN_FILE` is set. A dump from a server whose token
  lives on a Docker secret still restores to a working mirror. The new file
  name goes in the list `a_default_dump_carries_no_secrets`
  (`aurcache-utils/tests/dump.rs:960`) asserts is absent.
- **Load**: `load_secrets` (`restore.rs:167`) treats the new file like
  `tokens.json`: present without the CA is refused ("secrets travel
  together"), and present when the manifest says `includes_secrets: false` is
  refused (`a_manifest_that_lies_about_secrets_is_refused`). The rule is
  about how a private dump is produced, one flag and every secret, not about
  the token needing the CA. A dump with only some of its secrets is one
  that was assembled wrongly.
- **Restore**: `write_secrets` (`restore.rs:478`) writes
  `mirror-credentials.json` after the rows and the CA, under the same
  condition (`take_secrets`: `--secrets copy`, or `--clear`). Like the CA it
  is a file, so it cannot share the rows' transaction, and it is written once
  nothing is left that could roll back. If the target has
  `AURCACHE_MIRROR_GITEA_TOKEN_FILE` set, the token is still written to the
  managed file, and the restore reports that the external file takes
  precedence, so the restored token is not the one in use.
- **Merge without `--secrets copy`**: the target keeps its own token, as it
  keeps its CA. The preview says when the dump carries one that would be
  ignored, so that "why is my mirror not authenticating" has an answer
  before the restore rather than after.

### What was uploaded: carried in every dump

`mirror_uploads` is not authored, which is the export's usual test for
leaving a table out. But it is not derived either: it is the only record of
state held **outside** the instance, and no resolve or build can rebuild it.
Without it, a restored instance would find every file on the registry
"not ours":

- each rebuild at the version already on the registry gets a `409` with a
  different checksum, which is reported as a conflict and never resolved;
- the old files are never deleted, since only recorded uploads are deleted;
- a package removed after the restore stays on the registry for good.

So every dump, private or not, carries `mirror.json`: the recorded uploads,
keyed by target, with `pkgbase` rather than ids, like everything else in a
dump. The rows hold no secret, only filenames and checksums that the registry
serves publicly anyway. A restore writes them in the same transaction as the
packages, and `--clear` replaces them. In merge mode, a row is taken from the
dump when the target has none for that target and filename; otherwise the
target's row wins, since it describes a more recent upload. A row whose
`pkgbase` the dump does not carry is refused, as an orphaned setting is
(`a_setting_for_an_absent_package_is_refused`).

`DUMP_SCHEMA_VERSION` (`aurcache-common/src/api/dump.rs:23`) goes to 2. Both
new files are optional, so a version-2 importer still reads a version-1 dump
(no mirror, nothing to restore). An older importer, though, does not look at
files it does not know, and would restore a version-2 dump without its
mirror and without a word. The importer refuses a newer schema for exactly
that reason: "guessing at them is how a restore silently drops what it did
not understand" (`restore.rs:83`). Bumping the version keeps that promise.

### After the restore

Right after a full restore, `repo.db` is empty and the builds are queued.
Step 3 judges removal by the `packages` table, so the reconciler leaves the
registry alone. As each rebuild lands, its files are uploaded. A rebuild at
the version already on the registry has a different checksum (builds are not
reproducible), and the [same-version policy](#open-questions) decides what
happens next. With "replace", a full restore deletes and re-uploads every
package on the registry once. That is correct but noisy, and it weighs on
that open question.

A restore without the token (a default dump, or merge with the default
`ignore`) leaves the reconciler in a **waiting** state: configured, with no
credentials, and doing nothing at all, neither uploads nor deletes.
`doctor` and the repository page say so. Once the token is set, the first pass
starts from the restored records.

### Restoring onto a second server

A private dump restored onto a second, running server (a staging copy)
clones the CA, as `db-export.md` already warns. It now also gives that
server the token and the same records, so two instances reconcile the same
registry, each deleting what the other uploads. The restore preview adds
this to its warning whenever the dump carries mirror credentials. A staging
copy should restore with `--secrets ignore`, or turn `mirror.gitea.enabled`
off before setting a token.

The configuration names one registry. `target` in the table and the
reconciler's inputs are shaped so that a list could come later, but the
first cut has no list.

---

## Clients and visibility

- **`/repo/info`** (`aurcache-common/src/api/repo.rs:13`) gains
  `mirrors: Vec<MirrorInfo>`: the flavor, the stanza a client should write,
  the key URL, and the status (`pending` file count, last success, last
  error). `advertised` says which stanza `repo config` prints by default.
- **`aurcache-cli repo config`** (`aurcache-cli/src/repo.rs:82`) prints the
  advertised repository's stanza, `--from internal|gitea` overrides it, and
  for the registry it prints `SigLevel = Required` and the two `pacman-key`
  commands. `--install` fetches the key and shows its fingerprint before
  `--lsign-key`, as `signed-repository.md` proposes for AURCache's own key.
  A private owner needs credentials in the `Server` URL, and the CLI says so
  rather than writing a token into `pacman.conf` by itself.
- **`doctor`** checks the mirror after the queue: configured, reachable,
  authenticated, and not lagging (`pending` stays above zero for longer than
  one pass, or the last error is recent).
- **Events**: `mirror.uploaded` and `mirror.deleted` at debug severity,
  `mirror.failed` (with the HTTP status and body) and `mirror.conflict` (a
  `409` on a file that is not ours) and `mirror.lagging` as warnings, `mirror.stopped` (auth or
  quota) as an error. They go in `Event` alongside `repo.swept`
  (`aurcache-common/src/api/events.rs:572`).
- **UI**: the repository page shows each mirror's status. A package's page
  shows whether its current files are on the registry, pending, or
  conflicting.

---

## Where it runs

- **Core**, in `aurcache-utils/src/mirror/` (planner, reconciler, Gitea
  client), started by the composition root next to the schedulers, with the
  migration in `aurcache-db`. This is the proposal.
- **A plugin** (`plugins.md`, example 13): the reconciler needs only
  `repo.db` and the package files, both already served over HTTP, plus
  outbound HTTP to the forge and a little storage for its records. A WASM
  plugin could do the whole job. But plugins do not exist yet, and the
  per-package opt-out, the CLI's stanza and `/repo/info` all want core
  types. The reconciler is kept free of anything a plugin could not reach, so
  that moving it out later stays possible.
- **An external script** run from cron that downloads `repo.db`, diffs it
  against the registry and uploads with `curl`, needing no AURCache change at
  all. It is a fine stopgap, and the docs can show one. It cannot have the
  per-package opt-out or the status in the UI, and it has to keep its own
  record of uploads in order to delete safely.

---

## Tests

- **Unit**: the planner is a pure function of (listed entries, records, now,
  grace) → actions, with named-field structs. Cases: first upload; a version
  bump uploads now and supersedes the old version later; a removed package is
  deleted now; a downgrade deletes the shadowing version now; a same-version
  rebuild deletes and then uploads; `any` from two platforms is uploaded once;
  an opted-out package is deleted; a conflict on a file that is not ours is
  reported and left alone.
- **Reconciler against a fake registry** (an in-process HTTP server with the
  Gitea semantics above, including newest-wins and `409`): crash between an
  action and its record, then resume; `401` stops the reconciler; `5xx` backs
  off.
- **Dump and restore** (`aurcache-utils/tests/dump.rs`, beside the CA's
  tests): a default dump carries `mirror.json` and not
  `mirror-credentials.json`; a private dump carries both, and a `--clear`
  restore writes the token file at 0600 and the upload rows; a merge with
  `ignore` keeps the target's token; mirror credentials without the CA, or
  under a manifest that says no secrets, are refused; a token read from an
  external file is dumped and restored to the managed file; an upload row
  for a pkgbase the dump lacks is refused.
- **Reconciler after an outage**, against the fake registry: three versions
  published while it returns `503`, then it comes back. Only the last
  version is uploaded, dependencies before dependents, and the superseded
  file is deleted no sooner than the grace period **after that upload**, even
  when the outage lasted longer than the grace period. An audit after a
  file is deleted by hand on the registry uploads it again.
- **Reconciler after a restore**: with the restored records and an empty
  `repo.db`, a pass deletes nothing; with no token, a pass makes no request
  at all.
- **`test-e2e-gitea.sh`**: run `gitea/gitea` (and `codeberg.org/forgejo/forgejo`)
  next to the e2e compose. Create a user and token with `gitea admin`, enable
  the mirror, build `hello`, then from a client container add the key, sync
  with `SigLevel = Required` and install. Bump the version and check that the
  old file is still downloadable until the grace period ends. Run it only
  for changes to the mirror, like the other specialized suites.

---

## Open questions

- **Spike first:** does Forgejo really share Gitea's `409`, newest-wins and
  `any` behaviour? The planner relies on all three. Which minimum versions of
  each should be supported? Gitea's docs show the delete URL with
  `x86-64` as the arch segment; is that a typo for `x86_64`, or is the arch
  normalized?
- Should a same-version rebuild replace the registry's copy (proposed: the
  mirror lists what AURCache lists), or keep it and report the divergence?
  Clients that already have that version installed will not re-download it
  either way. A full restore rebuilds everything at the versions already on
  the registry, so "replace" re-uploads the whole registry once after each
  restore (see [After the restore](#after-the-restore)).
- Is gating dependent promotion on the mirror ever wanted (promote only once
  the dependency is on the registry)? It is the piece that a true "instead
  of" would need. Without a use for it, it is left out.
- Should several registries (two forges, or two owners on one) be supported
  from the start, or is one enough until someone asks?
- Should the per-package opt-out be the other way round (opt in), for an
  instance that mirrors only what it intends to share?
