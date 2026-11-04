# Signed packages and a signed repository

Status: **Proposed** · Last updated: 2026-09-28

Upstream issue: Lukas-Heiligenbrunner/AURCache#274 ("Feature: Signed packages &
repo"). It asks for three things:

- every package signed individually,
- each package's signature included in `repo.db`,
- `repo.db` itself signed,

so that a client can run with `SigLevel = Required` rather than `Never`. This
doc covers what a signature means here, where the key lives, and where signing
fits into `Repository`.

---

## Where things stand

- Every stanza AURCache hands out says `SigLevel = Never`: the one the CLI
  writes (`aurcache-cli/src/repo.rs:95`, with the `--siglevel` flag documented
  as "AURCache does not sign packages", `aurcache-cli/src/main.rs:531`), the
  worker template (`aurcache-api/src/worker.rs:134`) and the job's
  `pacman.conf` (`aurcache-utils/src/job_config.rs:75`).
- Most of the plumbing for detached signatures is **already there**, unused:
  - `Pkginfo` reads a sidecar `<pkg>.sig` if one exists, refuses armored or
    oversized ones, and base64-encodes it into `pgpsig`
    (`pacman-repo-utils/src/pkginfo/parser.rs:88-99`);
  - `Desc` writes `%PGPSIG%` from that field
    (`pacman-repo-utils/src/repo_database/desc.rs`);
  - `Repository::publish` moves a staged `<pkg>.sig` in with its package
    (`aurcache-utils/src/repository.rs:541`), retires it with its package
    (`:558`), and the sweep treats `.sig` files as belonging to their package
    (`:609`);
  - the worker's artifact scan and the upload check already skip `.sig` files
    (`aurcache-worker-core/src/artifacts.rs:62`,
    `aurcache-utils/src/publish.rs:525`).
- Nothing produces a `.sig`, and nothing signs `repo.db` or `repo.files`.
- `aurcache-deps` parses `desc` entries by hand because `alpm-repo-db` requires
  `%PGPSIG%` on v1 entries (`aurcache-deps/src/repo.rs:372`). Signing does not
  remove the need for that parser (an unsigned repository stays supported),
  but its comment will need updating.

So nearly all the work is the key, the signing step, and handing the key to
clients.

---

## What a signature says

The signature means "this file was published by this AURCache instance".
It does **not** mean the package is safe. AURCache builds whatever the AUR
PKGBUILD says, and signing does not make that recipe any more trustworthy.
The docs and UI should say that plainly. Whichever side signs, an approved
worker's output gets signed, a compromised worker's included. So "approve
this worker" stays the real trust decision, and signing does not replace it.

What any signature buys is protection against everything **between** the
signer and the client: a plain-HTTP repository (the common case on a LAN), a
reverse proxy or mirror in the middle, someone who can write to the served
directory but cannot run code on the server, and a stale or swapped
`repo.db`. The database signature also defeats the obvious attack on
`%PGPSIG%` itself, which is to rewrite the database so that it lists a
different signature.

---

## Who signs: the server or the worker

Two machines could hold a package-signing key. **Only the server can sign
the databases:** `repo.db` is written there, from changes made by several
workers. So the question applies to packages only.

### Server-side: one instance key

The server signs each artifact while publishing it, with the same key it
uses for the databases. Workers do not change at all: they upload unsigned
artifacts over mTLS, as they do now.

- **Clients trust one key**, added once, and it never changes as workers come
  and go. This is what the issue asks for, and what every
  `pacman-key --lsign-key` tutorial assumes.
- **A worker never holds a key**, so a worker lent by someone else, a worker
  on a laptop, or the hybrid image's embedded worker has nothing to leak.
- **A compromised server can sign anything.** Whoever runs code on the
  server holds the key, and can publish a new package that clients will
  accept.

### Worker-side: a key per worker

Each worker generates a key when it enrolls, next to its mTLS identity, and
signs its artifacts after the build, outside the chroot. The server checks
the signature at upload against the key recorded for that worker, publishes
it as `%PGPSIG%`, and signs only the databases with its own key.

- **Signatures carry provenance.** A package's signature says which worker
  built it, and still says so after it leaves AURCache (a mirror, a
  `pacman -U` of a copied file).
- **A compromised server cannot sign new code.** Without any worker's key it
  can only re-serve packages that were already signed, as a downgrade or a
  replay, and a correctly signed database does not stop that, because the
  server signs the database too. That is a real narrowing, and the only
  property here that server-side signing cannot offer.
- **A compromised worker exposes no more than it already does.** It could
  already upload anything and have it signed, so holding its own key gives
  it nothing new. Revoking a worker also revokes its key, and packages it
  signed stop verifying. That is what you want after a compromise, but it is
  disruptive when a worker is simply retired, because every package it built
  then has to be rebuilt or re-signed.
- **Clients need to trust a changing set of keys.** Adding each worker's key
  by hand does not scale, and it breaks whenever a worker is added. The
  workable form uses a certification hierarchy: the instance key is the only
  one clients `lsign` **and give full owner trust**, and the server certifies
  each worker key with it at approval. pacman's keyring then accepts any
  worker key the instance key has certified. This is how `archlinux-keyring`
  works (master keys certify packager keys), and like it, it wants a
  **keyring package** so that new worker keys and revocations reach clients
  through `pacman -Syu` instead of by hand.
- **The key has to be kept away from builds.** It is readable only by the
  worker's service user, which builds never run as. On the native package,
  though, that user has `NOPASSWD: ALL`, so the key is exactly as safe as
  the worker's root (see `unprivileged-workers.md`).
- **Every worker kind needs a signer**: the chroot worker, the docker
  builder, the hybrid image's embedded worker, and `build-once`.

### A middle ground: worker keys for provenance, the server key for clients

A worker signs its artifacts, the server verifies each signature at upload
and records which worker key made it, then signs the artifact again with the
instance key for publishing. This gives the server a provenance record
without any change to clients. But it gives no protection against a
compromised server, which is the one thing that makes worker-side signing
worth its cost. The mTLS upload already tells the server which worker sent
the file, so the extra signature adds little.

### Proposal

**Start server-side**, and leave room for per-worker keys:

- The first cut is the instance key, which is what the issue asks for and
  what clients can use today.
- The instance key is designed as the **root** from the start: an
  Ed25519 primary key whose job is to certify, with everything else layered
  under it. Per-worker keys can then be added later as keys the root
  certifies, and clients that trusted the root keep working.
- Per-worker signing becomes worth doing once there is a keyring package,
  because that is what makes a changing set of keys manageable for clients.
  Until then its main benefit, containing a compromised server, is outweighed
  by the extra cost for every user.

The rest of this doc describes server-side signing.

---

## 1. The key

One keypair per instance, as in the thread. It signs packages and databases
for every platform, and has no expiry. It is a certifying primary key from
the start, so that per-worker keys can later be certified under it
([Who signs](#who-signs-the-server-or-the-worker)). Rotation is an explicit
operation (see [Rotation](#rotation)); expiry would just be a date on which
every client stops upgrading.

### Algorithm and library

Ed25519, through the `pgp` crate (rPGP: pure Rust, MIT/Apache, the same
licences as this repository). This avoids depending on `gpg`: there is no
agent and no `GNUPGHOME`, nothing shells out, and no keyring directory has to
be kept consistent with the database. Sequoia is the other pure-Rust option.
It is LGPL, which is workable but a change of licensing posture for an
MIT-licensed binary, so it is not the default choice.

**To verify before committing to it:** pacman verifies through gpgme/GnuPG.
An rPGP-made Ed25519 key and detached signature must go through
`pacman-key --add`, `--lsign-key` and a `pacman -Sy` with
`SigLevel = Required`, in `test-e2e.sh`. If that fails, the fallback is RSA
4096 from the same crate, then `gpg` as a subprocess.

### Where it lives

Two candidates were raised on the issue:

| | file in the server's data directory | row in the database |
|---|---|---|
| survives a redeploy | yes, on the same volume as `repo/` and the CA | yes |
| in a DB export (`design/implemented/db-export.md`) | no | **yes**: an export would then carry the signing key |
| readable by someone with DB access only | no | yes |
| precedent | the worker CA's `ca-key.pem` (`aurcache-ca/src/lib.rs:20`) | none |

**File**, beside the CA key: `signing-key.asc`, mode 0600, created by
`load_or_create` in the same way. The CA already set the pattern for "a
private key the server must keep across redeploys", and the database is the
thing that gets exported, backed up to other places and inspected, so it is
the wrong home for the key. `AURCACHE_SIGNING_KEY_FILE` points somewhere else
for operators who want the key on a Docker secret or a separate mount; when
it is set, the server never writes the key, only reads it.

A passphrase (the suggestion on the issue) protects a file only from someone
who cannot also read the passphrase, and the server has to be able to read it
at every publish. It is optional (`AURCACHE_SIGNING_KEY_PASSPHRASE_FILE`),
never required, and the docs say what it does and does not protect against.

### Turning it on

Signing is **off by default** and turned on per instance
(`AURCACHE_SIGN=1`, or a global setting). Turning it on for an existing
instance must not break clients that are already configured:

- Clients on `SigLevel = Never` ignore signatures and keep working.
- Clients using the pacman default (`Required DatabaseOptional`) would start
  failing on any package that has no signature. So enabling signing also runs
  a **backfill** that signs every published file (next section) before the
  instance reports itself as signed in `/repo/info`.

Generating the key is lazy: the first time signing is enabled, the key is
created if the file does not exist yet.

---

## 2. Signing packages

This happens in `publish_build`, after the upload has been verified and
before `repo.begin()`. For each staged artifact:

1. Sign it and write the detached binary signature to `<staged>.sig` beside
   it, in the staging directory.
2. `Repository::describe(staged)` runs as it does today. `Pkginfo` picks the
   sidecar up and fills `%PGPSIG%` (existing code).
3. `Update::add` and `commit` follow as now. `publish` already renames the
   `.sig` in with the package.

The key is used outside the repository lock, so the lock is not held while a
signature is computed. Signing happens once per file and only reads the file,
which the server already does to hash it.

Both the `.sig` file and `%PGPSIG%` are published. pacman reads the signature
from the database when it is there, and the file serves clients that were
configured to ignore the database's copy, and `pacman -U <url>`.

### Backfill

The backfill signs everything that is published but unsigned. This covers
enabling signing for the first time, and a rotation.

It is one `Repository` update per platform: sign each listed file that has no
`.sig` (or has one made by a different key), then re-add each one with its new
entry. This is the same path as a rebuild at the same version, which replaces
the entry (`re_adding_replaces_rather_than_duplicates`). Signatures are
computed before `begin()`, and the update commits in one go, so `repo.db`
never lists a mix of signatures from two keys.

---

## 3. Signing the databases

pacman fetches `<repo>.db` and, when the `SigLevel` requires database
signatures, `<repo>.db.sig`. The same applies to `.files` for `pacman -F`.

The databases are signed in `prepare()` (`repository.rs:474`), which already
writes `.repo.db.tar.gz.next` and `.repo.files.tar.gz.next`. It also writes
`.repo.db.tar.gz.sig.next` and `.repo.files.tar.gz.sig.next`, and `publish`
renames each signature in **immediately after** its database. The two renames
are not atomic together, so a client can fetch the new database with the old
signature in the moment between them. It then gets a verification failure
for that one refresh, and the next `pacman -Sy` succeeds. That is the same
window `repo-add` has. The alternative is to publish databases under
versioned names behind a symlink swap; that is heavier, and pacman does not
need it.

The served names follow the existing symlinks (`repo.db` →
`repo.db.tar.gz`, `pacman-repo-utils/src/repo_init.rs`): add `repo.db.sig` →
`repo.db.tar.gz.sig` and the same for `files`, created by `init_repo` and
repaired at startup for repositories made before this change.

Since `prepare()` is inside the lock, database signing does happen under it.
Two signatures per platform per commit is negligible next to rewriting the
archive.

---

## 4. Getting the key to clients

A signature is only as useful as the client's way of knowing the key.

- **`GET /repo/key`** serves the armored public key without authentication,
  like the repository itself. **`GET /repo/info`**
  (`aurcache-common/src/api/repo.rs`) gains `signing: Option<SigningInfo>`
  with the fingerprint. A `None` means the instance does not sign. That is
  different from a key that is still being backfilled, which reports
  `backfilling: true`.
- **`aurcache-cli repo config`** prints `SigLevel = Required` and the two
  `pacman-key` commands when the instance signs, and keeps `Never` otherwise.
  `--install` runs `pacman-key --add` and `pacman-key --lsign-key <fpr>` (with
  the same `sudo` fallback it already uses for `pacman.conf`) after **showing
  the fingerprint and asking**. The fingerprint comes from the same HTTPS
  connection the token authenticates, but the prompt is what makes `--lsign`
  a decision the user made.
- **The web UI's repository page** shows the fingerprint and the stanza.
- **A keyserver** (`keyserver.ubuntu.com`, as in the thread) is an operator's
  choice and is never automatic: uploading a key is not reversible. The docs
  give the command.
- **A keyring package** (`aurcache-<name>-keyring`, like `archlinux-keyring`)
  would let `pacman` itself carry rotations. It is worth it once rotation
  exists, and is out of scope for the first cut.

### Workers

Workers install dependencies from this repository with `SigLevel = Never`,
both in the job's `pacman.conf` and in the worker template. Once signing
works, the job config should carry the fingerprint, and the worker should
import the key into the chroot's keyring and require signatures from the
repository. That closes the same man-in-the-middle gap for the build
pipeline. It is a separate step, after clients.

---

## Rotation

Rotation generates a new key, backfills everything with it, then publishes
the new fingerprint. Clients have to trust the new key before the backfill
lands, or they fail until they do. Without a keyring package there is no
smooth handover, so the first cut offers rotation only as an explicit
operator action (`aurcache-cli repo rotate-key`, admin-only) with a warning,
and it signs the new key with the old one so that a client that looks can
see the chain.

---

## Tests

- Unit (`pacman-repo-utils`): a signed entry's `%PGPSIG%` round-trips, and an
  unsigned repository is written as today.
- Unit (`aurcache-utils::repository`): a signed commit leaves each database
  with a matching `.sig` and each package with its `.sig`; a failed commit
  leaves all four signatures untouched; retiring removes the package's `.sig`
  with it (already covered).
- `test-e2e.sh`: enable signing, add the key with `pacman-key`, install from
  the repository with `SigLevel = Required DatabaseRequired`, then tamper with
  one byte of a served package and see pacman refuse it.

---

## Open questions

- Is rPGP's Ed25519 output accepted by the GnuPG version pacman links
  against? (Spike first; it decides the library.)
- Should the backfill run automatically when signing is turned on, or wait
  for a confirmation? A large repository takes a while to re-sign, and until
  it finishes `/repo/info` reports that it is still backfilling.
- When per-worker keys land, should the instance key keep signing packages
  from workers that have no key of their own (older versions, the docker
  builder), or refuse them? The first keeps mixed fleets working. The second
  is the only way to actually get the "a compromised server cannot sign"
  property.
- Per-worker trust, such as only signing output from workers the operator
  marks as trusted, would make the signature mean more. It also makes an
  unsigned package a normal state instead of an error. Not proposed; noted
  because the question will come up.
