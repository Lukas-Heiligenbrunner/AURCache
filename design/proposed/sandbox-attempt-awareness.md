# Design: Seeing what the sandbox denies

Making the PKGBUILD sandbox *aware of the attacks it turns away*. The
`aurcache-sandbox` Landlock wrapper confines attacker-supplied PKGBUILD code
and currently records nothing about what it stopped — a denied access is
indistinguishable from one never attempted. This design adds a report: which
path-bearing syscalls the policy *would* block, and which connections go to
undeclared hosts, surfaced per build. It deliberately **does not** extend the
denials beyond what the sandbox already enforces, and fails no build.

Status: **Proposed** · Last updated: 2026-09-15

This began as a comparison with a third-party build sandbox (`archon`, an eBPF
build monitor) whose ideas are summarized here rather than left to a
conversation nobody else can read:

- **Worth taking** — observing the *attempts* even when containment succeeds.
  That project flags a write to a read-only `/var/tmp` its own sandbox already
  refused, on the grounds that a real attacker's target might not have been
  read-only. The attempt, not the effect, is the signal that a PKGBUILD is
  hostile.
- **Worth taking** — sealing the off-`$HOME` channels (`/run/user/.../gnupg`
  agent sockets) that scrubbers like `env -i HOME=/nonexistent` do not close.
- **Not worth taking** — denying the network to *builds*, and the exec
  allow-list. Build-time network is routine for real packages (`unreal-engine`
  clones its whole tree inside `prepare()`; `aarch64-linux-musl-cross-bin`
  fetches its toolchain), and an execution allow-list is a judgment layer whose
  documented false positives (`makepkg` internals, namespace-setup writes) are
  its own drift problem. Strictness here must be **observation**, not
  interruption.

  The *parse* is the exception, and it is already denied: `aurcache-sandbox`
  grew `--no-net` and `--isolate-ipc`, and the server passes both when it
  sources a PKGBUILD, with the `parse_network` setting as the per-package
  escape hatch. Sourcing a recipe needs no sockets; building a package does.

## Motivation

PKGBUILD code executes in two places that are not a container, and both are
confined by `aurcache-sandbox` (`backend/aurcache-sandbox/src/main.rs`):

1. **The server parses a PKGBUILD.** `alpm-pkgbuild-bridge` parses by
   *sourcing* the file — every top-level statement runs, inside the process
   that owns the package database and the repository. The server runs it
   through `aurcache-sandbox` itself (`backend/aurcache-utils/src/pkgbuild.rs`,
   the `Bridge` type): Landlock makes the server's data unreadable and only the
   parse's own temp dir writable, the environment is scrubbed, and `--no-net`
   and `--isolate-ipc` deny TCP and signalling outside the sandbox. (Until
   recently this was a shell wrapper, `packaging/alpm-pkgbuild-bridge-wrapper`;
   both the wrapper and its `scripts/test-sandbox.sh` regression have been
   replaced by that Rust call site and its tests.)
2. **The worker fetches sources outside the chroot.** `makechrootpkg` runs
   `makepkg --verifysource` (and a direct `source PKGBUILD`) as the build user
   on the worker. The same Landlock binary grants write access to `SRCDEST`,
   `BUILDDIR` and the working directory, keeps everything else non-writable,
   and makes the worker's identity files unreadable
   (`--read-except`, `main.rs:36-57`).

Both are fail-closed and both are **silent**. Landlock is a pure deny: the
blocked syscall returns `EACCES`, and no kernel API reports that it happened —
Landlock emits no audit record and nothing was written to disk for inotify to
see. A malicious PKGBUILD that tries to exfiltrate the database is stopped; the
build log grows no line, the operator learns nothing, and a package that
*attempted* the same thing looks identical to one that never did. That is the
whole signal: the one observable that separates "this PKGBUILD is hostile but
contained" from "this PKGBUILD is puzzlingly badly written" is thrown away.

The point of this design is not to add more denials (that has diminishing
returns and breaks legitimate packages); it is to add the **attempt report**,
and to close the ambient read channels we know are open.

## Why "detecting the EACCES" is impossible after the fact

There is no hook to detect a Landlock denial:

- **No audit event.** Landlock denial does not enter the kernel audit trail on
  the kernels this project runs. Even were it added, auditd is a host-level
  service the server and worker do not ship — wiring it in would be a new
  privileged dependency purely for telemetry.
- **No filesystem signal.** `inotify`/`fanotify` report events that *happened*.
  A denied write never touches the path, so there is nothing to watch.
- **No socket/network signal either.** The parse now runs with `--no-net`, so
  a TCP attempt there is refused by Landlock and is just as silent as a denied
  write. The worker's fetch phase still has the network, by necessity, and an
  exfiltration attempt through it leaves no observable at all unless the
  destination is compared against what the package declared.

The correct move is therefore not to *observe* the denial but to **predict
it**: `aurcache-sandbox` already constructs the exact Landlock ruleset for each
run, so it already holds the predicate that decides what would be granted and
what would be denied. A supervisor that watches the confined child's syscalls
and runs each path against that same predicate can classify every access —
successful or not — as "inside policy" or "an attempt outside it", and only the
second class is worth reporting. We never need to know which errno Landlock
returned; we know a write to a non-granted path *is* the attack shape whether
the kernel refused it (`EACCES`), the target didn't exist (`ENOENT`), or the
supervisor didn't deny it at all. The predicate is the answer to "how do we
detect these EACCES issues": **we don't read the EACCES; we decide the path was
outside policy and that is the event.**

Keeping classifier and ruleset from drifting is then a structural requirement,
not a style preference: the predicate must be the *same function* that builds
the Landlock grants, so a widened grant list widens the classifier in the same
release. The current `read_grants_excluding` walking logic (`main.rs:277-307`)
and the writable-dirs computation (`main.rs:194-219`) are the natural extraction
points.

## Supervising the child: two mechanisms

The sandbox binary already owns the process launch (`main.rs:186`), so either
mechanism is a change to how it starts the child, not a new deployment.

### A. ptrace (the first cut)

Run the child with `PTRACE_TRACEME` and a seccomp filter returning
`SECCOMP_RET_TRACE` for the syscalls of interest (`openat`/`openat2`, `connect`,
`execve`, `mkdir`, `rename`, `unlink`, `truncate`, `chmod`). With
`PTRACE_O_TRACESECCOMP`, the supervisor gets a stop per relevant syscall—not one
per syscall—reads the register state with `PTRACE_GET_SYSCALL_INFO`, resolves the
pathname from child memory (`process_vm_readv`), classifies it, logs it, and
continues without altering the outcome.

Availability is worth stating rather than assuming: a process may always trace
a child it forked itself under Yama's default `ptrace_scope=1` (the value on
the machines this runs on), and `no_new_privs` does not interfere. A hardened
host at `ptrace_scope=2` or `3`, or a container without `CAP_SYS_PTRACE`, has
to fall back to mechanism B or run without a report -- which is exactly why the
report is off by default and the mechanism slot is pluggable.

Advantages: a pure-process tracing loop, no kernel negotiation, no
capabilities, and it composes with what the sandbox already does — and it is
the *only* one of the three mechanisms that works identically on every
deployment shape (native worker, container worker, hybrid image, and the rootless server parse). Costs: per-syscall stops are real overhead on a
syscall-heavy build, and thread handling — `make -j` spawns workers constantly —
needs the supervisor to keep following fork/thread events or the tracing
silently narrows. Neither is fatal for the phases that matter most: the server
parse and the worker `makepkg --verifysource` are near-single-threaded, and the
tracee set is small.

### B. seccomp-user-notify (the eventual path)

The child installs a seccomp filter with `SCMP_ACT_USER_NOTIF` for the same
short syscall set; the kernel pushes each matching syscall to a listener fd the
sandbox owns, and the supervisor thread answers each one after classifying the
argument. Same predicate, same logging, but the stops reach a normal thread
rather than ptrace and there is no per-thread tracing bookkeeping.

Costs: real complexity. Arguments arrive as registers, so relative paths need
`AT_FDCWD` resolution against a child cwd you must track; the filter must be
*joined* with any filter the outer container already installed (narrowing, not
widening); and the notify path is the wrong place to do anything slow. It is
the right mechanism once the report goes default; ptrace is the right one to
prove the predicate and the signal first.

Both are off by default. A `--report <path>` flag on `aurcache-sandbox` (or an
`AURCACHE_SANDBOX_REPORT` env, mirroring the existing `AURCACHE_PKGBUILD_BRIDGE`
indirection family) turns supervision on; without it the binary is byte-identical
to today.

## What the report classifies

Three event classes, all **report-only**, all keyed by the policy predicate:

1. **Writes outside the build's own directories.** The core event, and the one
   the Landlock policy already encodes. On the worker that is "outside the
   `SRCDEST`/`BUILDDIR`/cwd grant set"; on the server, "outside the parse's
   temp dir". This is archon's `WriteOutsideBuildDir`, produced without eBPF.
2. **Connections to undeclared hosts.** The `connect` hook, with the predicate
   supplied by the *data we already have*: the parsed `.SRCINFO` `source=()`
   hostnames and the configured mirror host are stored server-side per package,
   so the allowed destination set is known without running any PKGBUILD code —
   the same reason a static scanner reads `.SRCINFO` rather than sourcing the
   PKGBUILD. **Nothing is denied.** A build that needs the network keeps it
   (`unreal-engine`, `aarch64-linux-musl-cross-bin`); it is only logged if it
   talks to hosts it never declared.

   This class is the weakest of the three, and the design should say why:
   `connect` carries an address, the declaration carries a name, and the two
   are joined by DNS. A host behind a CDN answers with addresses that change
   between the resolution and the connection, several names share an address,
   and a redirect reaches a host nobody declared for a wholly legitimate
   download. So the predicate is: resolve the declared names once when the
   supervisor starts, treat that set (plus the mirror) as expected, and record
   everything else as *undeclared* rather than *unauthorized* — a line to read,
   not a finding to act on. A build that resolves names itself and connects to
   what it got is indistinguishable from one that does not; only the addresses
   are comparable.
3. **Reads of the secrets channels that exist off the filesystem deny-list.** A
   read of `/app` data is already `EACCES`; the report makes it *visible*. The
   channels Landlock cannot see at all (unix sockets over `/run`, see the
   hardening section) have their paths added to the protected set and are then
   caught by the same classifier.

Neither `execve` of unknown binaries nor "locally-built binary run" events get
an allow-list: that is archon's judgment layer and explicitly out of scope
(see "What we deliberately do not do").

One property the report must never be sold as: an enforcement boundary. A
supervisor that reads a path out of the child's memory and classifies it is
racing the child, which can rewrite that memory between the stop and the read
(the classic argument-substitution race). Landlock, which resolves the path in
the kernel, is what actually decides; the report only describes. This is fine
for a signal and disqualifying for a gate, and the distinction should survive
anyone later proposing "we already know, so let us block".

A report is a **counter, not a verdict**. It never fails the build and never
blocks publishing on its own: legitimate builds do borderline-looking things
(`make` probes `-w` targets, `configure` tests writability of `/tmp`), and a
heuristic that turned those into failures would break the fleet for the benefit
of nobody. The value is comparative — a package whose report says "27 denied
writes, 4 undeclared connections" is a package to look at, and one whose report
is empty has earned its silence.

## Changes

### 1. Extract the policy predicate (so the classifier and ruleset cannot drift)

`backend/aurcache-sandbox/src/main.rs`: pull the writable-set computation and
the read-except walk out into pure, unit-tested functions that return a
`Policy { writable: Vec<PathBuf>, readable: Option<Rule> }`, and have both the
Landlock ruleset builder (`restrict`, `main.rs:310-358`) *and* the classifier
consume that one value. This is the piece that keeps the report honest; without
it the classifier inevitably drifts and starts logging today's grants as
tomorrow's attacks.

### 2. Report mode in `aurcache-sandbox`

`--report <path>` / `AURCACHE_SANDBOX_REPORT`: supervise the child with ptrace
(mechanism A above), classify against `Policy`, append one JSON line per event
(`{syscall, path, access, governed, policy_verdict, errno}`) plus a trailing
summary (`{denied_writes, denied_reads, undeclared_connects, syscalls_seen}`).
No event, no flag in the report = the confined command ran inside its policy
cleanly. The errno field is informational — the *event* exists even when the
syscall succeeded (a read that was permitted by POSIX perms but not by policy,
a connect that resolved). This is the whole "attempt vs effect" distinction:
effects are already denied, attempts are what the report should keep.

### 3. Thread the reports into the build lifecycle

- The **server parse** path: `Bridge::command`
  (`backend/aurcache-utils/src/pkgbuild.rs`) adds `--report <dir>` for that
  source beside the arguments it already passes. The existing defenses
  (`--read-except`, the scrubbed environment, `--no-net`, `--isolate-ipc`) stay
  exactly as they are; the report only adds knowledge.
- The **worker fetch** phase: `packaging/patch-makechrootpkg.py` and the
  sandbox PATH shim that wrap `makepkg --verifysource` set the same env for the
  one source-fetch invocation. The report is stored under the job's staging dir.

A job's report is attached to the build row (small structured column, JSON, the
same encoding discipline as `source_data`/`split_packages`) and surfaced where
builds already surface things: a line under the build details — "⚠ 27 accesses
outside the build sandbox were attempted and denied; 4 connections went to
undeclared hosts" — with the event list expandable in the existing log viewer.

### 4. Harden the protected set (the off-`$HOME` channels)

The scrubbed environment (`HOME=/nonexistent`, `env -i`) defeats `$HOME`-based
keyring reads, but **not** process-independent socket paths. Archon's live-test
catalogue found exactly this class: gpg agent/keybox sockets under
`/run/user/<uid>/gnupg/` are resolved independently of `$HOME`, so a builder
with no keyring at all could still reach a running agent. The same applies to
`$XDG_RUNTIME_DIR` sockets and dbus. Add to the server's `--read-except` set
and the worker's `sandbox-protected` listing (`backend/aurcache-sandbox/src/
main.rs:82`):

- `/run/user/*/gnupg`, `/run/user/*/keyring`, `/run/user/*/bus` (per-uid runtime
  sockets), plus `/root` and `/home` wholesale.

One deliberate **exception that must not be "protected": the worker's ssh-agent
socket.** The worker intentionally hands builds `SSH_AUTH_SOCK` for private
`git+ssh://` sources (`backend/aurcache-worker/src/agent.rs:1-30`) — the agent
authenticates without ever making the key readable — and its socket directory is
bind-mounted into the chroot (`agent.rs:74`). The report would flag reads of it
as suspicious and be wrong; the protected-list additions are per-role for
exactly this reason. The ssh-agent boundary is a *designed* trust surface; the
gpg agent is not one.

### 5. Scope the worker's fetch sandbox too (`--isolate-ipc`)

The parse passes `--no-net --isolate-ipc`; the worker's source fetch passes
neither (`aurcache-sandbox --allow-build-env -- makepkg --verifysource`, from
`packaging/aurcache-patch-makechrootpkg`). `--no-net` must stay off there --
fetching sources *is* the network -- but `--isolate-ipc` should go on:

- **What it stops.** Landlock's IPC scoping (ABI V6, Linux 6.12) denies
  signalling processes outside the sandbox's own domain and connecting to
  abstract unix sockets outside it. Today a PKGBUILD's `source=()` handling
  runs as the build user and could signal, or reach an abstract socket of, the
  processes around it.
- **Why it is safe for a fetch.** The one IPC channel a fetch is *meant* to use
  is the ssh-agent for `git+ssh://` sources, and that is a pathname socket
  (`SSH_AUTH_SOCK` under `/run`), which scoping does not touch -- it restricts
  abstract sockets and signals, not filesystem sockets. Sockets a build creates
  for itself stay inside its own domain, where scoping allows them.
- **Kernel floor.** ABI V6 means Linux 6.12; the sandbox refuses a flag its
  kernel cannot honour rather than pretending, so an older host keeps today's
  behaviour instead of silently losing it.

This is a denial, not a report, and the one place this design widens the
policy: it closes a channel with no legitimate user, rather than guessing at
which network destinations or executables are legitimate.

### 6. A per-package network policy ladder (future, default report-only)

Not proposed for implementation yet — recorded here so the shape is known. The
natural extension of the connect report is a per-package setting for *builds*
(the parse is already denied, with `parse_network` to re-enable it), resolved
through the established `Package -> Env -> Global -> Default` precedence, that
says how aggressive the network policy is: `report` (today's behavior, plan
stop), or `deny-undeclared` (blocks connections not on the package's own
declared-source + mirror set, at the package's option, not globally). The
default stays `report` because build-time network is legitimate — see
`persistent-build-directory.md`'s `unreal-engine`, whose tree is cloned inside
`prepare()` — and because moving to denial on a schedule nobody controls would
quietly break every package that fetches what it needs mid-build.

## What we deliberately do not do

- **Deny network by default.** `unreal-engine`, `aarch64-linux-musl-cross-bin`
  and friends require it; the docker worker also deliberately shares the parent
  netns so `AURCACHE_PUBLIC_URL` stays reachable
  (`backend/aurcache-worker-docker/src/network.rs:8-19`). A global denial would
  be a fleet-wide regression sold as a security win. Observation first; per-
  package opt-in denial later.
- **Exec allow-lists / runtime ACLs.** Archon's own report history shows the
  allow-list false-positiving on `makepkg` internals and its own sandbox setup
  — the judge layer drifts, and every correction widens it. AURCache's position
  (documented in `main.rs:48-53`) is that enumerating what a build reads is
  open-ended and unmaintainable; the error of enumerating *what it runs* is
  identical in kind. The denied-access report gives the policy's sharpest
  signal without inventing a second policy to keep honest.
- **eBPF as the default (archon's choice), even though we have the root for
  it.** "The project avoids root" is not a real argument here: the chroot
  worker's sudoers is `NOPASSWD: ALL` and says so plainly — *"with NOPASSWD:
  ALL, code execution in the worker is root"* (`packaging/aurcache-worker.
  sudoers:15-16`) — and the docker worker holds the docker socket, which is
  the same thing. The boundary that keeps *untrusted code* from that root is
  not the worker's privilege; it is the `no_new_privs` that Landlock
  requires, which makes `sudo` inert inside the sandboxed PKGBUILD
  (`aurcache-worker.sudoers:24-26`). An eBPF monitor would be a *separate*,
  root-loading process exactly as in archon, with the confined build kept
  privilege-dropped — architecturally compatible with this project's model.
  The rejection is narrower and more honest: the package graph is a userspace
  classification (resolve path → run the `Policy` predicate → log), and
  seccomp-user-notify delivers exactly that with no kernel coupling, while
  eBPF adds aya objects, BTF/kernel-version build constraints, and capability
  requirements in the same container shapes those images ship — all to
  observe syscalls whose *meaning we compute in userspace anyway*. And on the
  one host where a root observer genuinely would be new — the **server**,
  whose parse path runs a PKGBUILD in the very process that owns the database
  and repository, with no sudo around it at all — eBPF would buy the same
  signal at the cost of a privileged process living beside the most valuable
  untrusted input this project has. If a deployment ever does want archon's
  full coverage, nothing in this design stops it: the classifier and event
  format are mechanism-agnostic, and the mechanism slot (ptrace, seccomp-
  user-notify, eBPF) is the one thing that changes.
- **Relying on a kernel audit facility.** Not available for Landlock today, a
  host service to ship, and invisible on the container deployments even where
  the kernel could produce it. The in-process supervisor is strictly more
  portable and keeps the event attached to the build that produced it.

## Consequences

- A contained attack becomes *visible*: the parse of a hostile PKGBUILD writes
  a report that survives in the source's or build's record even though the
  attack failed; a package that tries to phone home shows an undeclared
  connection line even though nothing was exfiltrated.
- The server's parse path keeps working verbatim. The wrapper's defenses are
  unchanged; a report is additive. A parse that would have succeeded still
  does.
- Network-dependent packages build unchanged. The connect report exists; the
  denial policy does not.
- The report is an extra syscall-dense pass only when requested. Non-report
  mode is the exact binary that exists today, so the steady-state cost is zero.

## Verification

- **Unit (predicate parity).** The extracted `Policy` classifies every grant as
  allowed and every protected path as denied — the existing test set for
  `read_grants_excluding` (`main.rs:420-483`) gains the mirror assertions on the
  classifier, so the two sides of the same function cannot disagree.
- **Sandbox regression (`backend/aurcache-utils/src/pkgbuild.rs` tests).**
  Those tests already run the hostile fixture through the real sandbox and
  assert the payloads are refused, including that a denied connection is
  distinguishable from one merely refused by the network. Extend them so the
  confined leg also asserts the *attempts* are recorded, and the unconfined leg
  that the classifier sees the same paths (an attempt exists whether or not
  Landlock stopped it). A benign fixture must produce an empty report, which
  keeps the counter honest.
- **Integration.** Run the phases with the report on: server parse of the
  hostile fixture yields `denied_writes > 0`; a worker fetch phase yields the
  summary JSON under the job staging dir; the build row surfaces the counter
  after ingest.
- **E2E.** A real package that uses build-time network (`hello` is not enough;
  use one of the offending examples if available) builds **successfully** under
  report mode and publishes — proving the report never fails a build — while
  its connect events, if any, are listed.
- **Perf smoke.** A report-mode build finishes within a small bound of the same
  build without it (the ptrace first cut only targets the two near-single-
  threaded fetch/parse phases, where the overhead is dominated by one bounded
  process).

Full gate: `just lint`, `just test`, `just test-browser`.