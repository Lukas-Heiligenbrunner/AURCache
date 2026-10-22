# Review: Configuring workers from the server

Review of [`worker-configuration.md`](../implemented/worker-configuration.md).

## Verdict

The proposal has a sound security boundary and picks the right Phase 1
transport.  An allowlist that excludes host-access settings is essential, and
using the existing heartbeat gives configuration changes an appropriate,
failure-tolerant delivery path without introducing persistent-connection
state.

Before implementation, the protocol needs to distinguish *delivery
acknowledgement* from *effective configuration*, and the document needs a
precise atomic-update model.  Without those, env-pinned settings and partial
edits will cause repeated configuration delivery or mixed configurations.

## Must resolve before implementation

### 1. `config_version` cannot mean both “received” and “effective”

The proposed heartbeat says the worker reports the version it has “applied,”
while the worker also resolves environment values ahead of server values.  For
an env-pinned key, the worker can successfully receive server revision 12 but
its effective value will deliberately not equal revision 12.  If the server
compares one version to its desired configuration, it will resend the same
payload forever.  That makes a local pin look like a failed deployment and
prevents a meaningful acknowledgement.

Use two separate pieces of state:

- `received_config_revision`: an opaque server-generated revision that the
  worker echoes after it has durably accepted and parsed the server payload;
  this controls retransmission.
- `effective_config`: a separately reported set of values, sources, and
  diagnostics; this controls UI visibility and any server scheduling fields.

The revision should change atomically for a fleet-default edit or a
per-worker edit, even if its implementation is a content hash.  It is a
revision of the server payload, not a hash of the worker's env-resolved
configuration.

### 2. Make a configuration save atomic, including its revision

`worker_settings` is a key/value table, but an operator will commonly save
several related fields together (for example concurrency, per-build memory,
and cache budget).  Updating rows one at a time allows a heartbeat between
updates to receive a configuration that was never an intended state.  This is
especially dangerous for a lower concurrency paired with higher per-build
memory limit.

Define the API's PATCH as a complete transactional operation: validate all
keys and cross-field invariants first, update/delete all changed rows in one
transaction, write one audit entry, then advance the affected configuration
revision.  A worker must receive a self-contained snapshot for that revision,
never a delta whose interpretation relies on packets it may have missed.

### 3. State the acknowledgement and rollback behaviour for rejected values

The server validates syntax, but the worker can still reject a value: a
future setting may be unsupported by the binary, enabling a cgroup controller
may fail on that host, or a value can violate a host-specific constraint.  The
current “treated as unset” rule is unsafe if it also acknowledges the desired
revision: the UI could show a change as applied while the worker silently runs
with a weaker default.

For each received revision, require the worker to report per-key status:
applied, locally overridden, unsupported, or rejected (with a bounded,
operator-safe error).  The server should retain and show the last attempted
revision and the last successfully received revision separately.  For safety
limits, a rejected update must preserve the prior usable runtime value rather
than falling back to an unlimited default.

### 4. Define safe runtime snapshots and concurrency transition semantics

The design says settings apply “next build” or “immediately,” but does not
define how concurrent claim/build tasks observe an update.  The current runner
creates a fixed `tokio::sync::Semaphore`; reducing its permits while all are
held is not equivalent to reducing capacity, because each completed build
returns its permit.  The server must also not advertise a new concurrency to
its scheduler until the worker confirms its local gate has adopted it.

Introduce a runtime-config snapshot selected when a job starts.  A job keeps
that snapshot for its lifetime; a new job sees one whole later snapshot.  For
concurrency, use a gate with a target capacity and a deficit that consumes
returned permits until the running count reaches the target.  Report the
effective capacity only after this transition is installed, and have the
server schedule from that reported capacity.

CPU limits need similar preparation: the current worker enables the cgroup
`cpu` controller at startup only when `WORKER_BUILD_CPUS` is configured.  A
later CPU-limit update must enable and verify that controller before a new
build is allowed to use `cpu.max`; otherwise the configuration change turns
subsequent builds into setup failures.

## Important design revisions

### Environment precedence needs an adoption path

Environment-wins is a reasonable local safety veto and preserves existing
behaviour.  It also means that most deployed workers—whose policy variables
are already present in `/etc/aurcache/worker.env` or Compose—will remain
unmanageable from the UI.  Make this explicit as an adoption workflow:
leave bootstrap and host-access settings local, remove policy settings from
the worker environment, then set their fleet/per-worker values on the server.
The UI should name the exact environment variable that pins a row and should
not present saving an overridden server value as an effective change.

### Resolve server layers on the server

The server owns fleet defaults and per-worker overrides, so it should resolve
those two layers before sending a snapshot.  The worker then only resolves
`environment > server snapshot > built-in default`.  This avoids exposing a
database model to workers, makes the payload smaller, and makes a payload
revision unambiguous.

### Do not make routing depend on re-registration

Priority and package affinity are consumed by server-side scheduling, not by
the executor.  Requiring a fresh registration after an update reintroduces a
restart-sized delay and is circular for a running worker.  If these values are
server-configurable, update the scheduler's desired routing state directly,
and show the worker's reported effective capability separately.  In
particular, package affinity should probably be per-worker only: a fleet
default would assign the same restricted package to every otherwise-unset
worker and defeat affinity routing.

### Design compatibility deliberately

Heartbeat responses carry cancellation today.  A newly added configuration
field must be optional/defaulted so an older worker can still deserialize the
response and honour `cancel`; conversely, the server must know the worker
version/capabilities and avoid considering configuration delivered to a
worker that cannot understand it.  Unknown future setting keys should be
reported as unsupported, not make the entire response fail.

## Implementation checklist

- Use partial unique indexes for `worker_settings`: a normal unique constraint
  permits multiple SQLite rows where `worker_id IS NULL`.
- Cascade per-worker overrides on worker deletion and include the table in
  dump/restore.
- Audit one save transaction, recording scope and changed keys rather than one
  noisy activity entry per key.
- Keep drain/pause out of `worker_settings`: it is a server lifecycle state
  that should stop new claims immediately and let active builds finish.
- Leave the legacy Docker worker env-only until it is removed; translating its
  incompatible units creates a second, weakly tested implementation.

With those protocol and state-management details added, the proposed
visibility-first rollout and heartbeat delivery are ready to proceed.
