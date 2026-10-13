---
sidebar_position: 10
---

# Worker protocol

This page is for writing a worker: what a worker and the server say to each
other, in what order, and with what timing. Each request and message is also
in the server's API reference, under **Worker**, at `/docs` or `/redoc` on any
instance. This page covers what that reference cannot.

The workers in this repository (`aurcache-worker`, `aurcache-worker-docker`,
the demo worker) all speak it through one crate, `aurcache-worker-core`, which
is the reference implementation.

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

Within a protocol, messages only change in ways a peer cannot notice: a field
added in either direction is optional, and its absence means what it meant
before. A receiver ignores fields it does not know. Anything else -- a field
becoming required or changing meaning, a new value in an enum a peer receives,
a route removed, a change in timing -- comes with the next protocol number.
The registration messages and the CA routes are exchanged before any protocol
is agreed, so they belong to none, and only ever gain optional fields.

## Running a build

1. **Claim.** `POST /api/worker/jobs/claim` answers `200` with a job, or `204`
   when there is nothing for this worker; ask again after a few seconds (ten,
   by default). The server picks the job from the worker's registration
   (architectures, affinity, priority, concurrency), not from the claim.
2. **Fetch the sources.** `GET /api/worker/jobs/{build_id}/source` returns the
   package's sources as a gzipped tarball, with any patches the operator made
   applied. The job says the rest: build flags, `makepkg.conf`,
   `pacman.conf`, PGP keys to import, the package names the build should
   produce, and the commits to check out for `git+` sources.
3. **Stream the log.** `POST /api/worker/jobs/{build_id}/logs` appends UTF-8
   text to the build's log, as the build prints it.
4. **Upload the packages.** `POST
   /api/worker/jobs/{build_id}/artifacts/{filename}`, once per file, streams
   the file as the body. Percent-encode the file name. Send `Content-Length`
   when known, so a file larger than the build may produce is refused (`413`)
   before any of it is sent.
5. **Complete.** `POST /api/worker/jobs/{build_id}/complete` reports the
   outcome (`succeeded`, `failed` or `canceled`) with whatever is known: exit
   code, a reason, peak memory, disk use, the commits `git+` sources were at.
   A success is accepted at once and the server publishes the uploaded files
   in the background; nothing after that is the worker's to fix. Reporting
   the same outcome again is answered `200` and changes nothing, so a report
   whose answer was lost can simply be sent again.

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
