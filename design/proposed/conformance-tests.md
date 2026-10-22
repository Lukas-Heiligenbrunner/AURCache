# Conformance tests for the packaged binaries

`check()` in the PKGBUILDs used to run `cargo test --workspace`: the whole
backend suite, in the dev profile, once per package — five full recompiles of
twenty crates to test code the package does not ship. That is CI's job, and CI
does it. `check()` is reserved for conformance tests instead: small black-box
checks that the binaries just built behave like the real thing, run against
the release artifacts in `build()`'s target dir, with no recompilation.

Status: **Proposed** · Last updated: 2026-09-27

## Ground rules

- **Test the artifact, not the source.** `check()` runs the binaries out of
  the release dir. Anything needing `cargo build` or `cargo test` again does
  not belong here.
- **Fast and offline.** A conformance suite that downloads the world or takes
  minutes will be `--nocheck`'d out of existence. Loopback only; no AUR, no
  crates.io, no Docker daemon.
- **Native only.** Like the old helper, skip with a reason on a
  cross-compile: foreign binaries cannot run here. `check()` must still pass
  (skip, exit 0) so cross package builds stay green.
- **One script per binary**, next to the packaging (e.g.
  `packaging/conformance/<name>.sh`), wired from each PKGBUILD's `check()`.
  Shared helpers (ephemeral port picking, `curl` retry loops) in one sourced
  file.

## `aurcache` (server)

The richest target: it boots, migrates, and serves three ports.

- Boot with a temp working dir and throwaway SQLite, assert it listens and
  `GET /api/health` (or the docs root) answers 200, then shut it down cleanly.
  This alone proves the binary links, the migrations apply, and the API
  stack comes up.
- Assert the embedded UI is present: the landing page serves HTML, not 404.
  (Catches a `static`-feature regression, which is exactly the kind of
  packaging-only breakage unit tests cannot see — the feature that failed
  would be the one the package builds with.)
- Assert the pacman repository endpoint answers on its port with an empty but
  well-formed database.
- Open question: the server listens on fixed ports (8080/8081/8083). A
  conformance boot needs either a port override (env or flag — preferred) or
  fixture ports with a free-port probe and retry. Do not bind fixed ports in
  `check()`; a builder running two packages at once would collide with
  itself.

## `aurcache-worker`

A worker without a server cannot do anything — which is itself the contract:

- With no `AURCACHE_URL` / enrollment token, it must refuse to start with a
  clear error and a non-zero exit, not a panic or a hang. Assert the message
  names the missing piece.
- `--help` (and `--version`, if it grows one) exits 0 and mentions the
  enrollment variables, so a packaging change that breaks arg parsing fails
  loudly.
- Anything deeper (claim a job, run a chroot build) needs a server and a
  privileged chroot: that is the e2e suite's job, not `check()`'s.

## `aurcache-worker-docker` (legacy builder)

Same shape as the worker, minus any future: it is deprecated and kept for
continuity.

- Same refuse-cleanly-without-config assertion and `--help` exit 0.
- No deeper coverage is proposed; effort spent here should go to migrating
  off it instead.

## `aurcache-sandbox`

The parsing sandbox: small, critical, and already covered in spirit by
`scripts/test-sandbox.sh`.

- With the bridge binary missing from its expected absolute path, it must
  refuse rather than fall back to `PATH`. Assert the refusal.
- A trivial PKGBUILD (static `pkgver`, no network) parses to the expected
  fields through the sandbox. This exercises the real confinement path —
  Landlock included — on the build host's kernel.
- A parse that attempts a write outside its directory, or a read of a
  protected path, is denied. (These mirror `test-sandbox.sh`; the
  conformance script may simply invoke the same cases against the packaged
  binary.)
- Caveat: Landlock needs a recent host kernel. The sandbox already refuses
  to parse rather than parse unconfined, so on an old kernel these cases
  must skip with a reason instead of failing.

## `aurcli`

The friendliest target: several commands need no server and no token.

- `aurcli setup compose --role backend -o -` emits a compose file to stdout
  that parses as YAML and contains the expected services. Offline, instant,
  and it covers the template end to end.
- `aurcli repo config` prints a `pacman.conf` stanza for the default URL
  without contacting anything. Assert the stanza shape.
- `aurcli completions bash` (and one other shell) emits a non-empty script.
- `aurcli --help` and `aurcli <cmd> --help` exit 0, so CLI reshapes that
  break the parser fail here rather than in users' hands.
- `aurcli doctor` against a dead URL must report unreachable and exit
  non-zero — not hang past a short timeout, not panic. (Timeout-bounded;
  keep it to seconds.)

## What is deliberately not here

- No unit or integration tests: `cargo test` stays in CI, where one run
  covers all packages.
- No Docker, no privileged operations, no AUR or crates.io access. The
  chroot the package builds in has network today, but depending on it makes
  the suite flaky by construction.
- No fixed ports, no fixed paths outside a temp dir. Parallel package
  builds must not trip over each other.
