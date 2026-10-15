---
sidebar_position: 10
---

# Worker protocol

This page is for writing a worker: what a worker and the server say to each
other, in what order, and with what timing. Each request and message is also
in the server's API reference, under **Worker**, at `/docs` or `/redoc` on any
instance. This page covers what that reference cannot.

The protocol is JSON over HTTPS and belongs to no language or codebase: this
page and the reference are what it is. The workers in this repository
(`aurcache-worker`, `aurcache-worker-docker`, the demo worker) all speak it
through one crate, `aurcache-worker-core`, which is one implementation of it.
Example messages for each protocol are kept under
`backend/aurcache-common/tests/fixtures/worker-protocol/`.

## Transport

Every route is under `/api/worker/` on the worker listener, port 8083, over
HTTPS. The server's certificate is signed by AURCache's own CA, which a worker
pins (see [Enrollment](#enrollment)). Messages are JSON, except the source
archive, log output and artifacts, as noted below.

Once approved, a worker authenticates with a client certificate (mutual TLS)
that the server signed. Only the enrollment routes work without one.

## Enrollment

1. **Pin the CA.** `GET /api/worker/ca` returns the CA certificate as PEM, and
   `GET /api/worker/ca/fingerprint` its SHA-256 fingerprint. A worker given a
   fingerprint (`AURCACHE_SERVER_CA_FINGERPRINT` for the workers here) checks
   it; one without trusts the CA on first use. Either way it keeps the CA and
   verifies the server against it from then on.
2. **Register.** The worker makes a key pair, then sends `POST
   /api/worker/register` with a certificate signing request and a description
   of itself: name, architectures, concurrency, priority, package affinity, its
   version and kind, the settings it accepts, and the range of worker protocols
   it speaks. The worker is identified by the SHA-256 fingerprint of its public
   key, so registering again with the same key updates the same worker.
3. **Wait for approval.** An operator approves the worker in the UI or with
   `aurcli worker approve`, unless the server approves it on its own (a shared
   enrollment directory, a pre-approved fingerprint, or an enrollment token;
   see [Managing workers](./managing.md)). The worker polls `GET
   /api/worker/register/{fingerprint}/status`. Once approved, the answer
   carries its signed certificate and the CA, and the worker switches to mutual
   TLS.

A worker registers again **every time it starts**, and before doing any work:
the server's record of its configuration comes from registration, and so does
the [protocol](#protocol-versions) the two agree on. If the server cannot be
reached, the worker waits and retries; it does not claim anything until a
registration has succeeded. Registering is idempotent and never changes a
worker's approval, so it is always safe: a revoked worker stays revoked.

The registration answer also carries a `[repo]` section for `pacman.conf`,
with the host left as `%AURCACHE_HOST%` for the worker to fill in with however
it reaches the server, so builds can use packages AURCache already built.
`repo_template` is absent when the instance publishes no repository.

## Protocol versions

What a worker and the server agree on is a protocol number, never a version of
either: workers are released apart from the server and may come from anywhere.
Only protocol 1 exists so far.

- The worker sends the protocols it speaks as a range, `{"min": 1, "max": 1}`,
  in `protocols` when it registers. The server answers with the newest one
  both speak, in `protocol`.
- No protocol in common: the registration is refused with `426 Upgrade
  Required`, and the body says what each side speaks. Nothing about the worker
  is recorded. Retry slowly (the workers here wait five minutes), because only
  upgrading one side changes the answer.
- The server holds the agreement in memory. After it restarts, any request
  from a worker it holds none for is refused with `428 Precondition Required`.
  The worker registers again and repeats the request. This is how every worker
  renegotiates with a server that was upgraded.

Within a protocol, messages only change in ways a peer cannot notice (see
[Reading messages](#reading-messages)): a field added in either direction is
optional, and its absence means what it meant before; a value added to an
enumeration is one its fallback reads acceptably. Anything else -- a field
becoming required or changing meaning, a value an older peer would misread, a
route removed, a change in timing -- comes with the next protocol number. The
registration messages and the CA routes are exchanged before any protocol is
agreed, so they belong to none, and only ever gain optional fields.

## Reading messages

What lets either side grow without a new protocol number is how both read
what they receive:

- **Unknown fields are ignored**, at any depth. A field a newer peer added is
  for peers that know it.
- **A missing optional field** means what it meant before that field existed.
  Optional fields may be sent as `null` or left out.
- **An unknown value of an enumeration** reads as that enumeration's
  fallback:

  | Field | Received by | Unknown value reads as |
  |---|---|---|
  | `status` in the registration answer | worker | `pending`: keep waiting |
  | `mirrorlist.mode` in a claim | server | `local`: send no mirrorlist |
  | `mirrorlist.offer` in a job | worker | `nothing`: use the image's own |
  | `outcome` in a completion | server | `interrupted`: retry the build |
  | `severity` in a worker log report | server | `warning` |
  | `kind.type` in a setting declaration | server | free text, unvalidated |

  A value may be added to one of these within a protocol when reading it as
  the fallback is acceptable to a peer that does not know it. The other
  enumerations (`applies`, `source` and `status` in the settings reports) have
  no fallback, so a value added to one of them needs the next protocol.

## Running a build

1. **Claim.** `POST /api/worker/jobs/claim` answers `200` with a job, or `204`
   when there is nothing for this worker; ask again after a few seconds (ten,
   by default). The server picks the job from the worker's registration
   (architectures, affinity, priority, concurrency), not from the claim.
2. **Fetch the sources.** `GET /api/worker/jobs/{build_id}/source` returns the
   package's sources as a gzipped tarball, with any patches the operator made
   applied. The job says the rest:
   - `makepkg_conf`: assignments to apply over the worker's own
     `makepkg.conf`, after it. Where things go (`PKGDEST`, `BUILDDIR`,
     `SRCDEST`, `SRCPKGDEST`, `LOGDEST`) and how many jobs run (`MAKEFLAGS`)
     are never in it: they are the worker's, set after it, and a line the
     operator wrote for one of them is the worker's to drop.
   - `pacman_conf`: the whole `pacman.conf`, without the AURCache repository.
     The worker appends the `[repo]` section from its registration answer,
     with the host filled in.
   - `build_flags`: arguments added to makepkg's command line, as given.
   - `pgp_keys` to import, and `vcs_sources`: the `git+` sources whose commit
     to report back.
   - `expected_packages` and `expected_version`: what the build should
     produce. A real build's packages are whatever makepkg makes; these are
     for a worker that fabricates them.
3. **Stream the log.** `POST /api/worker/jobs/{build_id}/logs` appends UTF-8
   text to the build's log, as the build prints it.
4. **Upload the packages.** `POST
   /api/worker/jobs/{build_id}/artifacts/{filename}`, once per file, streams
   the file as the body. Percent-encode the file name. Send `Content-Length`
   when known, so a file larger than the build may produce is refused (`413`)
   before any of it is sent.
5. **Complete.** `POST /api/worker/jobs/{build_id}/complete` reports the
   outcome with whatever is known: exit code, a reason, peak memory, disk
   use, the commits `git+` sources were at. Reporting the same outcome again
   is answered `200` and changes nothing, so a report whose answer was lost
   can simply be sent again.

   | Outcome | When | What the server does |
   |---|---|---|
   | `succeeded` | The build made its packages, and they are uploaded. | Publishes them in the background; nothing after that is the worker's to fix. |
   | `failed` | The build ran and failed (a non-zero exit, out of memory, a timeout), or its packages cannot be accepted: none, not a regular file, or refused on upload with `400` or `413`. | Fails the build. The package's doing, so it is not retried. |
   | `canceled` | The worker stopped it because it was asked to. | Nothing more; the build was already stopped. |
   | `interrupted` | The worker could not see it through, for reasons of its own: preparing it, fetching its sources, a crash, a lost connection while uploading. | Queues it again while it has attempts left, as for a worker that went silent. |

Problems a worker notices on its own account, which the server would not see
otherwise, go to `POST /api/worker/log`.

## Leases and heartbeats

A claim starts a lease on the build, 60 seconds by default (`LEASE_TTL`, the
same on both sides). The worker keeps it with `POST /api/worker/heartbeat`,
every 15 seconds by default, listing the builds it is running:

- Each listed build's lease is renewed.
- A build the worker holds but no longer lists is taken as dropped, and
  abandoned.
- A build whose lease runs out, because its worker went silent, is abandoned
  by the server, which queues a fresh build of the package when there are
  retries left.

A worker that cannot reach the server for longer than the lease stops its
builds itself: the server has abandoned them by then, and an upload after that
would be refused.

The heartbeat's answer also carries settings an operator set for the worker on
the server (`config`), whenever the worker does not hold the current revision.
The heartbeat reports which revision it holds and, when it changes, what each
setting resolved to on the worker (`received_revision`, `effective`).

## Cancellation

A build is stopped by an operator, or abandoned by the server. Either way, the
worker learns it from:

- the `cancel` list in the heartbeat's answer; and
- `GET /api/worker/jobs/{build_id}/status`, which a build can poll while it
  runs (every five seconds in the workers here) and which says
  `cancel_requested`.

The worker then stops the build and reports it `canceled`.

## Mirrorlists

A worker can use its own mirrorlist, or the one the server maintains. Each
claim says which (`mirrorlist`): `local`, or `server` with the checksums of the
mirrorlists it already holds, by architecture. The job answers with `send`
(the content, with its checksum to hold on to), `keep` (what the worker holds
is current), or `nothing` (the server has none for that architecture: drop
any held copy). The checksum is the server's own; a worker never computes one,
only sends back what it was given.
