# Versioned worker protocol, independently released components

Status: **In progress** (negotiation, docs and the v1 review done; three review items open in section 2) · Last updated: 2026-10-15

Workers and the server talk a numbered protocol, and that number is the only
thing either side checks. Each component -- server, chroot worker, container
worker, sandbox, CLI -- then has its own version and its own releases. A
worker fix ships without a server release, and a worker built in another
repository, with whatever version scheme it likes, works with any server
that speaks its protocol.

This has to land before the upstream merge ships a release. Until then
nothing on the wire has been deployed outside this fork, so v1 can still be
reshaped freely. Once a release is out, v1 is frozen.

---

## Where things stand

- **Nothing on the wire says which protocol it is.** `aurcache-common/src/worker.rs`
  says it in its header: "A worker and its server are released together, so a
  field is simply required on both ends". A mismatched pair fails as a JSON
  decoding error inside a retry loop, or worse, decodes and means something
  else.
- **Registration is best-effort after enrollment.** A worker that already
  holds a certificate registers on every startup, but carries on and claims
  jobs when that fails (`enroll.rs`, `ensure_enrolled`).
- **Versions are only displayed.** `RegisterRequest::version` and
  `Heartbeat::version` are stored on the `workers` row and shown on the Workers
  page. `aurcache-common/src/version.rs` says outright that nothing compares
  them, and that stays true.
- **The worker's version is wrong.** `aurcache-worker-core` reports
  `env!("CARGO_PKG_VERSION")` (`enroll.rs:41`, `runner.rs:387`), so every worker
  reports worker-core's `0.1.0`, whichever binary it is and whatever release
  it came from. `aurcache-worker`, `-worker-docker`, `-sandbox`, `-chroot` are
  all at `0.1.0`. Only `aurcache` (the server) is at `0.6.0`, which is what the
  AUR packages and the `v0.6.0` tag say for all of them.
- **There is one release line.** One tag (`v0.6.0`) feeds all five PKGBUILDs
  (`archive/refs/tags/v$pkgver`) and both image workflows.
  `docker-worker-publish.yml` insists that every image comes from the same
  commit, precisely because there is no protocol check.
- **Half the protocol is undocumented.** `WorkerApi`'s OpenAPI paths cover
  enrollment, the CA and the admin routes. They leave out claim, source, logs,
  worker log, artifacts, complete, heartbeat and job status: the part a worker
  from another repository most needs.

## 1. The protocol number

A single integer, starting at `1`. Each side speaks a contiguous range of
them, given as its oldest and newest: the server's is
`aurcache_common::worker::SUPPORTED_PROTOCOLS` (`1..=1` today), a worker's is
whatever its code handles. A range rather than a set, because a side that
speaks 1 and 3 but not 2 is not a case worth designing for.

### Negotiated at registration

`RegisterRequest` gains `protocols`, the oldest and newest protocol the
worker speaks (`{"min": 1, "max": 1}`). The server picks the highest protocol in
both ranges, keeps it in memory against the worker's certificate
fingerprint, and returns it in `RegisterStatus`. From then on, everything
that worker sends under its certificate is read as that protocol. No request
carries the number again.

Registration is where this belongs. It is where the worker already describes
itself (name, arches, concurrency, priority, affinity, version, kind), and the
protocol is a property of the worker, not of each request. That also covers
the routes with no JSON body (source, job status, artifact upload) without
anything added to them, and keeps the check in one place instead of on every
request.

### Registration comes before any work

A worker does no work until a registration has succeeded: no claim, no
upload, nothing beyond fetching the CA and registering. If the server cannot
be reached, the worker waits and retries with a backoff. This replaces
today's best-effort re-registration (`ensure_enrolled` in
`aurcache-worker-core/src/enroll.rs`), where a worker that already holds a
certificate logs a failed registration and starts claiming anyway. Under a
negotiated protocol that would let an upgraded worker, started while the
server was down, run under whatever protocol the server recorded for its
previous binary.

Waiting costs nothing: a worker that cannot reach the server to register
could not have claimed a job from it either. The compose case that made
registration best-effort, where the worker starts before the server listens,
becomes a few retries instead of an unregistered start.

Registration stays fast and idempotent. It is an upsert keyed on the
certificate's SPKI fingerprint that never touches approval, leases or builds,
so registering again is always safe: on every startup, after a settings
change (`sync_registration`), and after a refusal as described below.

### Refusing a worker

A registration with no protocol in common is refused:

- **Status:** `426 Upgrade Required`.
- **Body:** a plain-text sentence naming both ranges, for example "this
  server speaks worker protocols 1 to 1; this worker speaks 2 to 3".
  `send_checked` already puts
  the body into the worker's error, so the journal says why.

A registration without the range speaks nothing and is refused the same
way. Nothing has shipped without the field, so there is no pre-protocol
worker to allow.

The worker does not exit on a refusal, because systemd would only restart it
into the same refusal. It logs it once and keeps trying to register at a slow
interval, so a server upgrade is picked up on its own.

### Only in memory

The agreed protocol means something only to the server process that agreed
to it, so it is not stored, and there is no migration. After a server
restart, an upgrade included, the map is empty, and any request from a worker
it does not hold gets:

- **Status:** `428 Precondition Required`, "register first".

The worker registers at once and carries on. Leases and builds are in the
database and untouched by this, so builds running through a server restart
go on. An upgrade that drops a protocol needs nothing of its own: every
worker renegotiates with the new server the same way, and one it can no
longer talk to is refused as above. Dropping a protocol is still a release
note, never a silent change.

The two statuses ask for different things: `428` means register now, `426`
means no protocol in common, so wait and retry slowly.

A refused registration is turned away before anything about the worker is
recorded, so it never reaches the approval list, and a worker can only be
approved once a protocol was agreed. The server logs the refusal, and the
worker's journal carries the `426` body. Showing a worker's agreed protocol on
the Workers page can follow; nothing needs it while there is one protocol.

### What may change within a version

v1 is a contract with workers the server's authors never see, so it has rules:

- **Allowed within v1:** adding a field in either direction, as long as it
  is optional (`Option<_>` or `#[serde(default)]`) and the receiver behaves
  exactly as before when it is absent. Receivers ignore unknown fields; serde
  already does, and nothing may turn on `deny_unknown_fields`. Adding a route
  is allowed too, as long as a worker that never calls it loses nothing.
- **Requires v2:** a new variant in an enum either side receives
  (`MirrorlistOffer`, `MirrorlistPreference`, `BuildOutcome`, `ApprovalStatus`
  in `RegisterStatus`). So does a field becoming required, a field changing
  meaning or shape, a route removed or changing semantics, or a timing
  contract changing: lease length versus heartbeat interval, or when a lease
  ends.

The header comment in `aurcache-common/src/worker.rs` changes from "released
together" to these rules. Each protocol 1 message's JSON is committed as a fixture
that the tests encode and decode against, so a change to a type that would
alter v1 fails CI instead of shipping.

When v2 exists, the server keeps answering v1 for as long as it reasonably
can. Dropping v1 is a release note ("this server needs workers speaking v2 or
later"), not a silent break.

### `version` and `kind` stay opaque

A worker from another repository reports its own version, in its own scheme,
and names itself through `kind`, which is already free-form. The server stores
and shows both and never parses either. Compatibility is the protocol number's
job alone.

### Implementation, for now

With one protocol there is nothing to dispatch on, so protocol 1 is simply
the types in `aurcache-common/src/worker.rs`, used by both ends as today.
What this design adds is small:

- the `PROTOCOLS` range and `ProtocolRange::negotiate` in
  `aurcache-common/src/worker.rs`;
- the range in `RegisterRequest` and the agreed protocol in `RegisterStatus`;
- on the server, the in-memory map from fingerprint to protocol, filled by
  registration and read by the guard that authenticates workers (`WorkerAuth`),
  which answers `428` for a worker it does not hold;
- on the worker, registration before any work, and registering again on a
  `428`;
- the fixtures: `aurcache-common/tests/fixtures/worker-protocol/1/`, one file
  per message, checked by `aurcache-common/tests/worker_protocol.rs`.

No version is passed around, because no code yet behaves differently by
version.

The protocol number names a contract, not a set of types: no type is ever
named after a version, and the types keep evolving as they do today. A later
version will usually not need new types either:

- A field added with a default, that the receiver can do without, is not a
  new version at all (see above).
- A new version is needed when one side must know the other honours
  something, for instance the server relying on a field only a newer worker
  sends. The field is still optional in the type, and the code checks the
  agreed protocol only where the behaviour differs. That is the first time
  the number reaches any code.
- Only a change that cannot be expressed compatibly (a field changing
  meaning, an enum gaining a variant an older side cannot parse) needs the
  old shape kept beside the type, under a descriptive name, with the
  conversion as the one place that knows which protocol gets which.

`RegisterRequest`, `RegisterStatus` and the CA routes stay outside any
version, since they are exchanged before one is agreed: they only ever grow
by optional fields.

## 2. Pre-freeze review of v1

v1 freezes with the release, so anything awkward should be fixed before then:

- **Make the spec complete.** Add the missing routes to `WorkerApi`'s OpenAPI
  paths: claim, source, logs, worker log, artifacts, complete, heartbeat, job
  status. Then `/docs` is the reference for a worker written elsewhere.
- **Document the protocol in prose.** Add `docs/docs/workers/protocol.md`
  covering what OpenAPI cannot: enrollment and mTLS, the lease and heartbeat
  timing, cancellation, the mirrorlist exchange, artifact upload order, and
  the evolution rules above.
- **Read each message once more as a contract.** Look for anything that
  mirrors this server's internals rather than describing the job: field
  names, `Vec<String>` arches and build flags, `JobDescriptor::packages`,
  the settings declarations (`SettingDecl`). Each later fix to one of these
  costs a v2.

### What the reading found (2026-10-15)

**The protocol is not the Rust types.** It is JSON over HTTPS, defined by
`docs/docs/workers/protocol.md` and the fixtures, and any language can speak
it. The types in `aurcache-common` are this repository's implementation, shared
by the server and the workers because both want the same shape today. When one
end wants something else, that end gets its own type, converted at its edge,
and the wire does not change. So `ApprovalStatus`, `Severity`, `DiskUsage` and
`KeptBuild` stay shared with the REST API for now.

**Reading rules.** Both sides ignore unknown fields at any depth, and every
enumeration a peer receives names the value an unknown one reads as
(`#[serde(other)]`, or a field-level reader for `Severity`, whose variant order
is its ranking). A value can then be added within a protocol when its fallback
is an acceptable reading. The fixture test decodes every sample with unknown
fields added, and checks each fallback.

Done:

1. **`interrupted`**, an outcome for "the worker failed, not the build". Setup
   failures, crashes and lost uploads report it. The server abandons the build
   with `EndReason::Interrupted`, which counts against `MAX_ATTEMPTS` like a
   silent worker. Packages that cannot be accepted (none, not a regular file,
   refused with `400`/`413`) still report `failed`, through `PackagesRefused`.
2. **`makepkg_conf`** holds the operator's assignments and the server's
   (`PACKAGER`, `OPTIONS+=(!debug)`), and nothing else. `PKGDEST` and
   `MAKEFLAGS` are the workers'.
3. **Names.** `RegisterRequest::affinity`, `JobDescriptor::expected_packages`
   and `expected_version`, and an optional `RegisterStatus::repo_template`.

Still open, and cheap only until the freeze:

4. **The lease is configured twice.** Server and worker each read
   `LEASE_TTL` (`aurcache-worker-core/src/config.rs:225`) and the protocol
   page asks for them to match. Sending the lease and heartbeat interval in
   `RegisterStatus` would make timing the server's to set.
5. **Two cancel channels.** The heartbeat's `cancel` and
   `GET /jobs/{id}/status` say the same thing. A route can never be removed
   after the freeze.
6. **The quarantine repository.** The hold design (`package-suspicion-signals.md`,
   "Chains") lets builds use packages held from the repository, through a
   second, opt-in pacman repository on the same port.
   The registration answer gains `quarantine_template`, rendered like
   `repo_template`, and a worker appends it after `[repo]`. A worker that
   did not would fail a held chain's first builds, so the rule belongs in
   protocol 1, before the freeze.

Additive, so not blocking:

- Appending to a log is not idempotent: a retried `POST .../logs` whose
  answer was lost duplicates output. An optional offset can come later.
- `CompleteReport` does not list what was uploaded, so the server publishes
  whatever is staged. An optional manifest (names, sizes, digests) can come
  later.

Found on the way and fixed separately: `OPTIONS=(!debug)` replaced the
chroot's whole `OPTIONS` array, so every package was built without `strip`,
`zipman` and `purge`.

## 3. Component versions

Each released binary owns its version. Library crates don't carry a
meaningful version: they get `publish = false` and keep whatever number they
have, since nothing reads it.

| Component (AUR package) | Crate whose version it reports | Image |
|---|---|---|
| `aurcache-server` | `aurcache` | `aurcache-server` |
| `aurcache-worker` | `aurcache-worker` | `aurcache-worker` |
| `aurcache-worker-docker` | `aurcache-worker-docker` | -- |
| `aurcache-sandbox` | `aurcache-sandbox` | -- |
| `aurcache-cli` | `aurcache-cli` | -- |

All five start at `0.6.0`, the version the server, the AUR packages and the
`v0.6.0` tag are already at. From there each moves on its own: a worker fix
releases the worker alone. Versions may keep matching when components are
released together, but no workspace-wide version enforces that.

`aurcache-worker-core` stops reporting a version of its own. The binary passes
in its own `full_version(env!("CARGO_PKG_VERSION"))`: through the worker
config for `aurcache-worker` and `-worker-docker`, and likewise for the demo
worker. The value reaches `RegisterRequest` and `Heartbeat` from there.

### Tags

A release is tagged `<package>-v<version>`, for example
`aurcache-worker-v0.6.1` or `aurcache-server-v0.7.0`. Several components can
be tagged on the same commit. The existing `v0.6.0` and older tags stay as
they are, as history.

`assemble_version` currently treats any `v*` tag on the build commit as "this
is a release". It changes to "a tag naming *this* component at *this*
version". The build script collects every tag pointing at `HEAD` into
`AURCACHE_GIT_TAGS`; image builds pass the one tag they were triggered by, as
they do today. A server built from a commit tagged only
`aurcache-worker-v0.6.1` then reports `0.6.0+g<sha>`, which is accurate: it
is not the `0.6.0` release. Tarball builds (the AUR) carry no git
information and report the bare version, as before.

### Packaging

Each PKGBUILD downloads its own tag:
`$url/archive/refs/tags/$pkgname-v$pkgver.tar.gz`. A worker release touches
only `packaging/aurcache-worker`, so the other packages see no update.

The sandbox is an interface of its own: the server and the workers execute
it with arguments and read what it prints. Within a major version that
interface only grows, and the packages that use it declare
`depends=('aurcache-sandbox>=X.Y')`.

### Images and workflows

| Tag pattern | Builds |
|---|---|
| `aurcache-server-v*` | `aurcache-server` (`docker-build.yml`); `aurcache` hybrid (it bundles the server) |
| `aurcache-worker-v*` | `aurcache-worker`; `aurcache-demo-worker` |
| `aurcache-worker-docker-v*` | `aurcache` hybrid (it bundles the container worker); `aurcache-builder` |

Each image is tagged with its component's version and `latest`.
`docker-worker-publish.yml` splits into one job per image, each filtered on
its tag pattern. The rule that every image comes from one commit goes away,
because the protocol check replaces it.

The weekly cron that refreshes Arch images currently rebuilds the default
branch as `latest`. With independent releases, `latest` should be the
component's last release, rebuilt on a fresh base, so the cron checks out the
newest `aurcache-worker-v*` tag (and so on) instead of `master`.

The hybrid image bundles two components, so its tag follows the server's
version. A container-worker-only release rebuilds it under the same tag.

### Releasing

`scripts/release.sh <package> <version>` bumps that crate's `version`,
refreshes `Cargo.lock`, commits, then tags `<package>-v<version>` and pushes
the tag. CI does the rest.

- **Images:** built by the workflows above.
- **AUR:** a workflow on the same tag patterns runs in an `archlinux`
  container and does the following:
  1. Work out the package and version from the tag.
  2. In `packaging/<package>`, set `pkgver`, reset `pkgrel`, run `updpkgsums`
     and `makepkg --printsrcinfo > .SRCINFO`.
  3. Build the package with `makepkg`, so a broken PKGBUILD never reaches the
     AUR.
  4. Commit and push to `ssh://aur@aur.archlinux.org/<package>.git`.
  5. Commit the new gitlink back to `master`, or open a PR for it if branch
     protection refuses the push.

  This has to happen after the tag, because the tarball the PKGBUILD downloads
  only exists once the tag does. So the gitlink update is a follow-up commit,
  never part of the release commit.

The PKGBUILDs carry a real checksum of the tag tarball, as the AUR guidelines
expect for non-VCS sources, so a bump is `pkgver`, the checksum and
`.SRCINFO`. `updpkgsums` does the checksum, so in CI it costs nothing.

**The AUR key.** AUR SSH keys are per account, not per package, so the
key in CI can push to every package that account maintains. It is a dedicated
key, added to the account for this alone so it can be revoked on its own. It
lives in a GitHub environment (`aur`) whose required reviewer is the AUR
maintainer, so a tag pushed by anyone else waits for their approval before
anything reaches the AUR.

**One-time switch.** The PKGBUILDs still name the fork
(`url="https://github.com/gyscos/AURCache"`) and download `v$pkgver` tags.
The switch happens after the upstream merge, and the PKGBUILDs need a tag in
this repository to point at. Upstream's newest tag is `v0.5.0`. Either push
the fork's `v0.6.0` tag here (its commit is in the merged history) and change
only `url` and the checksum, or make the switch the first per-component
release.

## 4. The CLI and the REST API

The CLI talks the REST API, not the worker protocol, so it gets no
protocol number from this design. Releasing it separately raises the same
question for REST, though. For now, keep REST unversioned, with the same
additive-only rule for fields, and have the CLI show the server's version in
`doctor` (`GET /api/version` already exists). A REST version can follow
the day a breaking change is needed.

## Order

Before the upstream merge leaves draft, because protocol 1 freezes with it:

1. Negotiation at registration, held in memory; registration before any
   work; the `426` and `428` answers and the worker's handling of them.
   **Done.**
2. The protocol docs: the job routes in `WorkerApi`'s OpenAPI paths (a test
   keeps every protocol route there) and `docs/docs/workers/protocol.md`.
   **Done.** The message-by-message reading of section 2 has not been done as
   a pass of its own; whatever it would have found costs protocol 2 once v1
   ships.

With the first release after the merge, since nothing depends on them before
one is cut:

3. Component versions: `version` per binary crate, all at `0.6.0`;
   worker-core takes it as a parameter; `assemble_version` matches component
   tags. Until then every worker reports worker-core's `0.1.0`, which is only
   what the Workers page shows.
4. Tags, PKGBUILD sources, split image workflows, `release.sh` and the AUR
   workflow, with its environment and key. The upstream maintainer has to
   agree to the tag scheme, and possibly create the environment.

## Open questions

- **`426` or `409`?** `426 Upgrade Required` formally expects an `Upgrade`
  header naming a protocol to switch to. It is still the most self-explaining
  status for this, and the body carries the details.
