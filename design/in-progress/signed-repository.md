# Signed packages and a signed repository

Status: **In progress** (server-side signing of packages and databases, the
backfill and key publication done; the CLI's and web UI's trust flow, rotation,
workers verifying, a keyring package and a staged rollout open — see [What is
left](#what-is-left)) · Last updated: 2026-11-20

Upstream issue: Lukas-Heiligenbrunner/AURCache#274 ("Feature: Signed packages &
repo"). It asks for three things:

- every package signed individually,
- each package's signature included in `repo.db`,
- `repo.db` itself signed,

so that a client can run with `SigLevel = Required` rather than `Never`. This
doc covers what a signature means here, where the key lives, and where signing
fits into `Repository`.

---

## What is built

- `backend/aurcache-signing`: the instance key (`InstanceKey`): generated,
  stored, loaded, unlocked, and signing. rPGP, no `gpg`.
- `Repository::with_key` (`aurcache-utils/src/repository.rs`): `describe`
  signs each package before the lock, `stage_databases` signs each database
  it writes, and the plan moves each signature in right after what it signs.
  `init` links `repo.db.sig`/`repo.files.sig`, or removes them when the
  repository no longer signs.
- `aurcache_utils::signing::Backfiller::run` and the background job that runs
  it (`aurcache-scheduler/src/signing_backfill.rs`): packages published
  unsigned, or under another key, signed through `Update::resign`, and
  databases no update has signed since, through `Update::rewrite_databases`.
- `GET /api/repo/key` and `GET /api/repo/signing` (no authentication), and
  `signing` in `GET /api/repo/info`.
- Docs: `docs/docs/Configuration/pacman-repo.md#signing-the-repository`, three
  rows in the environment page.
- `scripts/test-signed-repo.sh`: pacman itself, in a container, against a
  repository the server's code wrote.

---

## Where things stood

Before this, every stanza AURCache handed out said `SigLevel = Never` or
`Optional TrustAll`, and most of the plumbing for detached signatures was
already there, unused: `Pkginfo` read a sidecar `<pkg>.sig` into `%PGPSIG%`,
`Repository` moved a staged `<pkg>.sig` in with its package, retired it with
it, and the sweep treated `.sig` files as their package's. Nothing produced a
`.sig`, and nothing signed `repo.db` or `repo.files`. So the work was the key,
the signing steps, and handing the key to clients.

---

## What a signature says

The signature means "this file was published by this AURCache instance".
It does **not** mean the package is safe. AURCache builds whatever the AUR
PKGBUILD says, and signing does not make that recipe any more trustworthy.
The docs and UI say that plainly. Whichever side signs, an approved
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

### Decision

**Server-side**, with room for per-worker keys:

- The first cut is the instance key, which is what the issue asks for and
  what clients can use today.
- The instance key is built as the **root** from the start: an Ed25519
  primary key that only certifies, and an Ed25519 subkey under it that
  signs. Per-worker keys can be added later as keys the root certifies, and
  clients that trusted the root keep working. (Built: a generated key is
  `[C]` + `[S]`, and the unit tests check that GnuPG gives the subkey's
  signatures full validity through a local signature on the primary key.)
- Per-worker signing becomes worth doing once there is a keyring package,
  because that is what makes a changing set of keys manageable for clients.
  Until then its main benefit, containing a compromised server, is outweighed
  by the extra cost for every user.

The rest of this doc describes server-side signing.

---

## 1. The key

One keypair per instance. It signs packages and databases for every
platform, and has no expiry: expiry would just be a date on which every
client stops upgrading. Rotation is an explicit operation (see
[Rotation](#rotation)).

### Algorithm and library

**Built:** the `pgp` crate (rPGP 0.21, MIT/Apache, pure Rust), with its
default `bzip2` feature off — nothing here reads or writes compressed
messages, and it would be the only C library linked for OpenPGP. `rand` 0.8
comes along because rPGP's API takes its RNG traits. Nothing shells out to
`gpg`: no agent, no `GNUPGHOME`, no keyring directory to keep consistent.
`cargo check -p aurcache-signing --target armv7-unknown-linux-gnueabihf`
builds with no C toolchain, so the cross builds need nothing new.

Sequoia was the alternative. It is LGPL, a change of licensing posture for an
MIT-licensed binary, and its crypto backends (Nettle by default, or OpenSSL)
are C libraries the armv7 and aarch64 cross builds would have to provide.

The keys are **v4, algorithm 22** (rPGP's `Ed25519Legacy`, what GnuPG calls
`ed25519`), not RFC 9580's Ed25519 (algorithm 27): GnuPG 2.4, which pacman
verifies through, does not read the latter.

**Verified** (the spike the proposal asked for):

- `aurcache-signing`'s tests import the public key into a throwaway GnuPG
  home, `gpg --verify` a detached signature (`VALIDSIG` naming the primary
  key), see a tampered file give `BADSIG`, then do what `pacman-key --init`
  and `--lsign-key` do and see the subkey's signature reach full validity.
  They skip, saying so, where `gpg` is not installed. A second test loads keys
  `gpg --quick-gen-key` made and `--export-secret-keys` exported, with and
  without a passphrase.
- `scripts/test-signed-repo.sh` runs pacman 7.1 in an Arch container against
  a repository written by the server's code: `SigLevel = Required` installs a
  backfilled package and one published signed; a re-compressed `repo.db` is
  refused; a package carrying another package's signature, in its entry and
  beside it, is refused with database signatures not checked.

The RSA and `gpg`-subprocess fallbacks were not needed.

### Where it lives

| | file in the server's data directory | row in the database |
|---|---|---|
| survives a redeploy | yes, on the same volume as `repo/` and the CA | yes |
| in a DB export (`design/implemented/db-export.md`) | no | **yes**: an export would then carry the signing key |
| readable by someone with DB access only | no | yes |
| precedent | the worker CA's `ca-key.pem` (`aurcache-ca/src/lib.rs:20`) | none |

**Built as proposed: a file**, `signing-key.asc` in the CA directory
(`aurcache_common::fs::ca_dir()`, so the hybrid image's adopted CA directory
too), armored, written owner-only through `fs::write_private`, created by
`InstanceKey::load_or_create` the first time the server starts with signing
on. The database is what gets exported, backed up elsewhere and inspected, so
it is the wrong home for the key. Where the hybrid image keeps its CA in the
repository (`repo/.ca`), the key is there too, and as unreachable as the CA
key through the server's file server and the compose files' nginx, which
both refuse dot paths -- but not through whatever else serves, mirrors or
syncs that directory. So the server warns at start when the key's path is
inside `REPO_ROOT`, and the docs say to move the CA directory onto a volume
of its own or supply the key from elsewhere.

`AURCACHE_SIGNING_KEY_FILE` points at a key the operator supplies (a Docker
secret, a separate mount); the server only reads it, and refuses to start
when it is missing. It may be armored or binary.

**One signing subkey, chosen once.** The server signs with the key's one
current signing subkey -- bound for signing, neither revoked nor expired, its
secret in the file -- else with the primary key if that signs. A key whose
old subkeys have expired signs with its one current one, and a key exported
with `gpg --export-secret-subkeys` (a stub where the primary's secret was)
signs with its subkey. The choice is made when the key is loaded
(`choose`), and refused there, saying which, for a key in which nothing
signs, a key with **several** current signing subkeys -- the refusal says to
export only the one to use (`gpg --export-secret-subkeys <subkey>!`) -- and a
key whose signer stops signing within **`MIN_VALIDITY_DAYS` (30) days**. An
expiry is then something an operator deals with at a restart, weeks ahead,
rather than something clients find by refusing what was signed; a server that
runs on past it stops signing (`InstanceKey::sign` refuses), so publishing
fails rather than publish signatures clients reject. Thirty days is the
notice the previous design gave before an expiry, and longer than most
servers go between restarts.

Moving from one signing subkey to the next while the server runs -- a
handover a day before the old one expires, and the backfill signing
everything again with the new one -- was built and taken out: it needs the
clients to have imported the new subkey beforehand, which nothing tells them
to do, and it carried a second backfill trigger, a daily notice and a choice
made again at every signature. It belongs to a rotate-key operation (see
[Rotation](#rotation)), which is not built.

A subkey signs only under a primary key that is in force (`primary_expiry`):
GnuPG, and pacman through it, takes a subkey's signature under an expired
primary key for an expired one, and under a revoked primary for a revoked
one, whatever the subkey's own binding says. So a key whose primary key has
expired or is revoked is refused, saying which, and a subkey's signing ends
when its primary key expires if that comes first -- which the 30-day check
counts, naming the primary key. The primary key's expiry is read from its
newest self-signature that says one: a direct-key signature or a user ID
certification (types 0x10–0x13 and 0x1F), never a revocation. **Finding:** a
primary key that had expired years before, over a signing subkey that never
expires, was taken for a current signer, the whole repository was signed
again with it, and pacman refused all of it; `gpg --quick-gen-key` gives a
primary key three years by default, so supplied keys reach this on their
own. Tested with GnuPG keys: an expired primary, a revoked one, and one that
expires before its subkey, whose signature GnuPG accepts now and reports
`EXPKEYSIG` for once the primary has expired.

Only the key's own signatures are checked (`self_signed`): a certification
by someone else, which `gpg --export-secret-keys` includes, would fail rPGP's
`verify_bindings` for want of its issuer.

rPGP 0.21 refuses about one unprotected Ed25519 secret in 256 that GnuPG
(2.4.9 included) writes: it checks the secret's checksum over its own,
canonical encoding of the MPI, and GnuPG writes a secret whose first byte is
zero with a bit count of 256. Not fixed upstream as of 0.21.1; reported as
[rPGP #901](https://github.com/rpgp/rpgp/issues/901). rPGP's error is the one
a damaged file gets, so the server refuses either at load with one message
naming both, and what to try: a copy locked with a passphrase, whose checksum
is over the decrypted bytes, which rPGP gets right, else a backup. Reading the key anyway, by rewriting its checksum
before rPGP sees it, was built and taken out: about 130 lines of packet
parsing for one key in 256, which a passphrase avoids. The tests that load
keys GnuPG made run again with new keys when they hit one
(`with_gnupg`).

RSA keys sign through the `rsa` crate, which `pgp` brings and which has an
unfixed timing side channel (RUSTSEC-2023-0071); the server warns about an
RSA signer, and the docs recommend Ed25519, which it generates.

A passphrase protects the file only from someone who cannot also read the
passphrase, and the server reads it at every publish. It is optional
(`AURCACHE_SIGNING_KEY_PASSPHRASE_FILE`, one final newline ignored): a key
the server creates is locked with it, a supplied one unlocked with it. The
key is test-unlocked at start — sign and verify an empty message — so a wrong
passphrase fails there, not at the first publish. The docs say what it does
and does not protect against.

The secret key never leaves the crate: there is no accessor for it,
`InstanceKey`'s `Debug` shows the fingerprint only, and only the armored
public key is served.

### Backups

**Decided: the key is never in a dump**, with secrets or without
(`DumpSecrets`' doc, a test in `aurcache-utils/tests/dump.rs`). A dump with
secrets already carries the worker CA, but the signing key is worse to leak:
every client that trusts it installs, as root, whatever it signs, from
anywhere, for as long as it is trusted — long after the instance a dump came
from is gone. It moves as the file it is, copied on purpose; the docs tell
operators to back it up with the CA directory.

### Turning it on

**Built: `AURCACHE_SIGN=true`**, read once at start, through
`aurcache_common::server_env` and `settings::parse_boolean`. Not a global
setting as well (a deviation from the proposal): the key is deployment
state — a file on a volume, perhaps a secret — and turning signing on or off
at run time would need the key created, the backfill started or stopped and
the databases' signatures removed while the server runs. A restart does all
of that in a known order. A value that is not a boolean, or a key that will
not load, stops the server: running unsigned instead would take the
databases' signatures down, and every client requiring them with it.

**Finding: clients that do not trust the key fail, even under `Optional`.**
pacman checks every signature it finds; `Optional` only tolerates a missing
one. So from the first start with signing on, a client still on the
documented `SigLevel = Optional TrustAll` refuses the repository
(`key "…" is unknown`) until it trusts the key — `test-signed-repo.sh`
asserts exactly that. Only `SigLevel = Never` clients are unaffected. The
proposal assumed the opposite. The docs carry a warning.
pacman names the signing *subkey* in that error, not the fingerprint clients
are told to trust; the docs say so.

Startup order (`aurcache/src/main.rs`): load the key, `Repository::recover`,
`Repository::init` (links, or removes, the databases' signatures), resume
interrupted publications, then start the backfill in the background. Nothing
waits for it.

---

## 2. Signing packages

**Built in `Repository::describe`**, rather than in `publish_build` as
proposed: every package a signing repository adds is described first, so no
path can add one unsigned. It runs before `repo.begin()`, in the blocking
task that already reads the whole archive:

1. Sign the staged file and write the detached binary signature to
   `<staged>.sig`, through a dot-named file renamed over it — a new file, so
   a `.sig.publishing` link an earlier attempt left never changes under it.
   Whatever a worker uploaded under that name is replaced: what is published
   is always the server's signature.
2. `describe_package` picks the sidecar up and fills `%PGPSIG%`.
3. `Update::add` and `commit` as before; the plan moves the `.sig` in with the
   package, and retiring takes it out with it.

**Finding: both copies are needed.** pacman 7.1 verifies `%PGPSIG%` when the
entry has one, but still downloads `<pkg>.sig` and fails the transaction when
it is missing. So the detached files are not optional extras for
`pacman -U`; they must be served, and kept for retired files through the
grace period, as they are.

### Backfill

**Built** (`aurcache_utils::signing::Backfiller::run`), signing every entry
that does not carry the key's signature: published before signing, or under
another key (`Repository::unsigned`). An entry counts as signed when its
`%PGPSIG%` names the component key that signs as its issuer and the `.sig`
beside its file holds the same bytes -- without reading the file it signs:
the repository only ever lists a signature it made over the file it
describes. **Finding:** checking each signature over its file instead read
the whole repository at every start, minutes of IO on a large one, with
`backfilling` true all the while and clients told to keep signatures
optional. What the cheaper check cannot see is a file changed behind the
repository's back after it was signed; pacman refuses that one, as it should.

Per platform:

1. Read `<name>.db` without the lock and pick the entries to sign.
2. For each, link the published file (copy, where there are no hard links)
   into `.staging/resign/<platform>/`, and `describe` the copy — which signs
   it. A copy, so that whatever an update does to the published name, this
   one keeps the file that was signed.
3. `begin()`, read `<name>.db` again under the lock, and `Update::resign` each
   copy whose entry is the one picked -- the same `%SHA256SUM%` -- and whose
   file is what that entry describes. Retired files are skipped, files
   published again meanwhile were signed by their publication (checked by
   issuer only, cheaply, under the lock), and anything else is reported.
4. `commit` with no SQL: the plan moves each `.sig` in and the databases over,
   signed. `Update::resign` replaces an entry and moves only the signature;
   the package file never moves.

Each update writes the platform's databases again, whatever it signs, so the
batch grows with the repository: at least 64 packages, and at most 8 updates
per platform (`batch_size`). A fixed batch of 64 rewrote `repo.files` once
per 64 packages, which made the first backfill of a large repository
quadratic. The floor bounds how much a filesystem without hard links copies
at once in a small one. Then a platform whose databases do not carry the
key's signature -- an empty one, or one nothing needed re-signing on -- gets
an update that changes nothing and writes them again
(`Update::rewrite_databases`), so the databases are only ever signed through
the plan, as every update signs them.

The job (`aurcache-scheduler`) runs it at start, and again after 1 minute,
doubling to an hour, while it leaves anything unsigned; each run records
`repo.signed` and `repo.signing_incomplete` events. `/repo/info` reports
`backfilling: true` until a run finds nothing left (`Repository::mark_signed`).

A listed file that can never be signed does not keep it going: one not on
disk -- what `Finalization::finish` leaves when a staged file was lost before
its move -- one that cannot be read as a package, one that is not the file
its entry describes, and an entry with no `%SHA256SUM%` to tell. Signing it
again would fail the same way, and no client could install it either. The
backfill reports each once (a `repo.file_failed` event, action `sign`) and
does not count it as unfinished, so it neither keeps the job retrying hourly
nor keeps `backfilling` true and clients off `Required`. **Finding:** a
missing file used to do both, for good, and so would a damaged one.

**Open question resolved:** the backfill runs automatically. Turning signing
on already breaks untrusting clients at once (above), so holding signatures
back for a confirmation would not spare anyone; it only lengthens the time
`Required` clients cannot be configured.

---

## 3. Signing the databases

**Built in `Finalization::stage_databases`**, under the lock, beside what it
already wrote: `.repo.db.tar.gz.next` is signed into
`.repo.db.tar.gz.sig.next`, the same for `files`, and the plan moves each
database and then its signature. Replayed after a crash, the plan moves the
signature in as it does the database
(`a_replayed_plan_moves_the_database_signature_in_too`).

**The window.** A database and its signature are two files, and no rename
replaces both. pacman downloads `repo.db` and then `repo.db.sig`.

- Database first, then signature (built): a client mixes generations only if
  both its requests fall between the two renames, or its database download
  began before the first and its signature request came after the second.
- Signature first: a client mixes generations whenever its download span
  overlaps the window at all — a strict superset.

So the database goes first, and `Finalization::finish` renames a file and
its detached signature back to back, making the directory durable once after
the pair rather than between them (a sync between them would widen the
window to a disk flush). A client caught in it fails that one refresh with a
signature error, and its next `pacman -Sy` succeeds; `repo-add` has the same
window. Publishing under versioned names behind a symlink swap would close
it, at the cost of a scheme pacman does not need. The window is in the docs'
Limits.

**Served names**: `repo.db.sig` → `repo.db.tar.gz.sig` and the same for
`files`, linked by `pacman_repo_utils::repo_init::link_signatures`, called
from `Repository::init` — so repositories made before this get them at the
next start. A link to a signature not written yet answers 404, which is what
a missing signature is.

**Turning signing off** removes the databases' signatures and links at the
next start (`remove_signatures`): the databases are written unsigned from
then on, and a stale signature beside them would be refused even under
`Optional`. Package signatures stay; they still verify.

---

## 4. Getting the key to clients

A signature is only as useful as the client's way of knowing the key.

- **`GET /api/repo/key`** (built) serves the armored public key without
  authentication, as `application/pgp-keys`, and 404 when the instance does
  not sign. **`GET /api/repo/info`** (built) carries
  `signing: Option<SigningInfo { fingerprint, backfilling }>`; `None`, or a
  server that predates it, means unsigned. **`GET /api/repo/signing`** (built)
  answers the same without authentication, for a client with no token yet.
- **`aurcli repo config`** (not in this change; its own, the client trust
  flow) prints `SigLevel = Required` and the two
  commands (`curl … | sudo pacman-key --add -`, `sudo pacman-key --lsign-key
  <fpr>`) when the instance signs and has finished backfilling, and keeps
  `Optional TrustAll` — still printing the commands — while it has not.
  `--install` trusts the key *before* writing the stanza, after showing the
  fingerprint and asking (`sudo` unless root); without a terminal it does not
  ask, prints the commands, and writes no section unless `--siglevel` is
  given, since pacman would refuse it even under `Optional`. `pacman-key
  --add` is given `-` for the pipe: with no argument at all, pacman-key 7.1
  refuses. Without a token, it asks `/repo/signing`; with one, `/repo/info`,
  even with `--port`, since the answer is about more than the address. The
  fingerprint comes over
  the same connection the token authenticates; the prompt is what makes
  `--lsign` a decision, and lsigning by the reported fingerprint means a
  served key that is not that one is never trusted.
- **The web UI** (not in this change, with the CLI's): a *Repository signing* section on the Settings page
  with the fingerprint, the backfill state, the commands and a link to the
  key. There is no repository page to put it on. `test-frontend.sh` runs its
  server with signing on, so the browser checks render the section in its
  widest form (the first run caught it widening the page on a phone).
- **A keyserver** stays an operator's choice; the docs give the commands and
  say it cannot be undone.
- **A keyring package** — not built; see [What is left](#what-is-left).

### Workers

Not built. Workers install dependencies from this repository with
`SigLevel = Never`, both in the job's `pacman.conf` and in the worker
template. The job config should carry the fingerprint, and the worker import
the key into the chroot's keyring and require signatures from the repository.
That closes the same man-in-the-middle gap for the build pipeline. It touches
the worker protocol (an optional field) and every worker kind, so it is its
own change.

---

## Rotation

Not supported yet. The server signs with one subkey for as long as it runs,
and refuses a key with several, or one about to expire (see [Where it
lives](#where-it-lives)), so nothing moves from one signing key to another
under it.

Replacing the key is a restart with another one -- a new `signing-key.asc`,
or a supplied key -- after which the backfill signs everything again with it,
since a signature another key issued counts as unsigned. Clients refuse
whatever a key they have not imported signed, so the safe order is: every
client imports, and trusts, the new key first, *then* the server restarts
with it. A new subkey under the same primary key needs the same: clients
already trust the primary key, but have to import the key again to know the
subkey. During that backfill `repo.db` lists a mix of old and new signatures
(batches), which only matters to a client that trusts one of the two keys.

Still to build, as a follow-up: `aurcli repo rotate-key` (admin-only, with a
warning), which generates the new key or subkey, certifies it with the old
one so a client that looks can see the chain, publishes it ahead of use, and
hands signing over to it while the server runs -- with the backfill signing
everything again before clients stop accepting the old one. Ideally through
a keyring package, which is what would make rotation smooth.

---

## Tests

Built:

- `aurcache-signing`: sign/verify round trip, another key's signature
  refused, the generated key's shape, creation and reload with mode 0600,
  passphrase locking and wrong passphrases, passphrase files, garbage key
  files left alone, `Debug` without secrets, GnuPG verification and trust
  (above), GnuPG-exported keys, GnuPG keys whose primary key has expired, is
  revoked, or expires before its signing subkey, keys with two signing
  subkeys or one that stops signing within 30 days, a key past its expiry
  making no signature, and a key rPGP misreads refused with the way around
  it, which a GnuPG check then follows.
- `pacman-repo-utils`: `%PGPSIG%` round-trips from a sidecar through the
  databases and `listed_packages`; an unsigned package has none; signature
  links made and removed.
- `aurcache-utils::repository`: a signed commit signs each package and both
  databases; an unsigned one writes no signature; a failed signed commit
  leaves every signature as it was; a replayed plan moves the database
  signature in; `init` links or removes; an update that only rewrites the
  databases signs them; a package counts as signed only when its entry and
  its `.sig` agree; a worker's `.sig` replaced.
- `aurcache-utils::signing`: the backfill signs what was published unsigned
  and leaves the packages untouched, replaces another key's signatures, does
  not give a file replaced after its copy the copy's entry, and is a no-op
  for an unsigned repository; signs databases nothing else would, once; a
  listed file missing from disk, one that is not a package and one that is
  not the file listed are reported once and do not keep it from completing;
  the batch grows with the repository.
- `aurcache-utils/tests/publish.rs`: a signing server publishes a build with
  its signature, in its entry and beside it, and a signed database.
- `scripts/test-signed-repo.sh`, with its fixture
  `aurcache-utils/tests/signed_repository.rs` (ignored by `cargo test`).

Not built: signing in `test-e2e.sh` (a worker-built package installed with
`SigLevel = Required DatabaseRequired`). `test-signed-repo.sh` covers what
pacman makes of the server's output without workers.

---

## What is left

- **Workers verifying** the repository they install dependencies from
  ([Workers](#workers)).
- **The client trust flow**: `aurcli repo config` and the Settings page
  telling a client how to trust the key ([Getting the key to
  clients](#4-getting-the-key-to-clients)), its own change.
- **Rotation** as an operation -- a rotate-key command, more than one signing
  subkey and the handover between them, and certifying the new key with the
  old one ([Rotation](#rotation)).
- **A keyring package** (`aurcache-<name>-keyring`), so that rotations and,
  later, per-worker keys reach clients through `pacman -Syu`.
- **A staged rollout**: a mode that creates and publishes the key without
  signing yet, so clients can trust it before signatures appear and nobody
  sees the refusal described under [Turning it on](#turning-it-on).
- **Signing in `test-e2e.sh`**, and `test-signed-repo.sh` in CI.
- **Per-worker keys**, certified by the instance key, once there is a
  keyring package.

---

## Open questions

- When per-worker keys land, should the instance key keep signing packages
  from workers that have no key of their own (older versions, the docker
  builder), or refuse them? The first keeps mixed fleets working. The second
  is the only way to actually get the "a compromised server cannot sign"
  property.
- Per-worker trust, such as only signing output from workers the operator
  marks as trusted, would make the signature mean more. It also makes an
  unsigned package a normal state instead of an error. Not proposed; noted
  because the question will come up.
- Is a staged rollout worth a third value of `AURCACHE_SIGN` (say, `key`),
  or is "trust the key right after turning it on" enough for the instances
  this serves?
- Resolved: rPGP's Ed25519 output is accepted by GnuPG 2.4 and pacman 7.1
  (v4, algorithm 22). The backfill runs automatically.
