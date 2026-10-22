# AGENTS.md

This file provides guidance to AI coding agents (including Claude Code — `claude.ai/code`) when working with code in this repository.

## What this is

AURCache is a build server and repository for Arch Linux packages sourced from the AUR. Backend and
frontend are both Rust. Users add AUR (or git) packages for building; AURCache builds them in
containers and serves the results as a pacman repository, and detects when packages are out of date.

`frontend-rs/` (Dioxus, compiled to wasm) is the frontend. A Flutter UI used to live in `frontend/`; it
was removed once it had diverged past usefulness, and `git log` is where to find it if ever needed.

## Packaging lives in AUR submodules

`packaging/aurcache-{sandbox,worker,server,worker-docker,cli}` are **git submodules**
of the AUR repositories the packages are published from, so the PKGBUILD in the
tree and the one users install from cannot drift. A fresh clone has empty
directories until:

```bash
git submodule update --init
```

The image builds and `scripts/build-packages.sh` read those PKGBUILDs, so both
refuse with an explicit message rather than failing obscurely when they are not
checked out. CI checks them out with `submodules: recursive`.

They clone over `https://aur.archlinux.org/<pkg>.git` and push over
`ssh://aur@aur.archlinux.org/<pkg>.git` (`pushurl` in `.gitmodules`), so cloning
needs no AUR account and only a maintainer can publish.

## Build, test, and lint commands

```bash
# repo helpers (Justfile at repo root) -- these are the whole workflow
just serve         # run a server with the UI embedded, as the container ships it
just format        # cargo fmt, both workspaces
just lint          # clippy over both workspaces; the frontend for wasm *and* host
just test          # cargo test, both workspaces (no browser)
just test-browser  # scripts/test-frontend.sh
just test-kernel   # scripts/test-kernel.sh (docker, privileged)
just clean

# Rust backend workspace
cd backend
cargo fmt -- --check
cargo clippy -- -D warnings
cargo check
cargo test --all

# run a single Rust integration test
cargo test -p aurcache-db --lib m20261008_000000_from_0_5
cargo test -p aurcache-utils --test add scenario_b_one_aur_dep

# Rust frontend (its own workspace: it only builds for wasm32-unknown-unknown,
# so including it in the backend workspace would break `cargo build --workspace`)
cd frontend-rs
cargo clippy --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo test          # host target; the component logic, not the browser

# There is no separate frontend build step. `aurcache-api`'s build script
# compiles the frontend to wasm and embeds it under the `static` feature,
# re-running whenever the frontend changes.
cd backend && cargo run --features aurcache-api/static -p aurcache

# builds used elsewhere in the repo
cd docs && yarn install --frozen-lockfile && yarn build

# Rust frontend in a real browser: asserts every route mounts (~1 min)
./scripts/test-frontend.sh
./scripts/test-frontend.sh --shots /tmp/shots   # also write screenshots
./scripts/test-frontend.sh --online             # include routes that fetch from the AUR

# end-to-end smoke test
./scripts/test-e2e.sh hello

# build the Arch packages from this tree (and optionally install them)
./scripts/build-packages.sh --install
./scripts/build-packages.sh --packages aurcache-server

# build (and push) the container images: server, worker, hybrid. Same thing the
# publish workflow does. `--toolchain-repo` points armv7 at a pacman repository
# that already has the cross toolchain, turning an hour into seconds.
./scripts/build-images.sh <registry> --push --toolchain-repo 'http://host:8081/$arch'
./scripts/build-images.sh --images server --platforms linux/amd64 <registry>
./scripts/build-images.sh --packages-dir /tmp/pkgs <registry>   # also keep a host copy of the AURCache packages the images built (host arch; replayed from the build cache, not rebuilt)
```

### Which end-to-end suite to run

`scripts/test-e2e.sh` (the split server/worker path, ~4 min) is the default and
is what CI runs nightly. Reach for the others only when a change touches what
they cover, rather than running all four as a matter of course:

- `test-e2e-hybrid.sh` — for changes to the hybrid image itself (its
  entrypoint, the embedded worker's loopback enrollment, the Podman it runs in
  DinD mode) or to the legacy docker builder, which is its only builder.
- `test-e2e-hybrid-legacy.sh` — the same image in host mode, against the
  host's Docker socket: for changes to how the builder shares its build
  directory with a daemon outside the container.
- `test-sandbox.sh` — only for changes to `aurcache-sandbox` or the PKGBUILD
  sourcing path.
- `test-kernel.sh` — for changes to `aurcache-chroot`, the worker's pool or
  cache volumes, or its cgroups. It runs the tests `cargo test` leaves out
  (`AURCACHE_POOL_TESTS`, `#[ignore]`d cgroup tests) as root in a privileged
  container. Needs Linux 6.7+ for the pool, and Cargo's target directory on
  local storage: root in the container cannot write to a root-squashed NFS.

CI runs the lint and unit tests for both workspaces, `test-frontend.sh`,
`test-kernel.sh` and `test-sandbox.sh` on every push; `test-e2e.sh` runs
nightly instead, and only when something landed that day. It cannot cover a
pool on a block device or zvol, or an image on ZFS; the runner has neither.
Add the `run-e2e` label to a pull request to run the e2e suite there on demand.

Unit and integration tests are cheap (backend ~8s, frontend <1s) and should be
the reflex. They render components directly, though, so they cannot see whether
the page boots — `test-frontend.sh` is what covers that, and every rendering
defect this frontend has had was of that kind.

## High-level architecture

- `backend/` is a Cargo workspace. `backend/aurcache` is the composition root: it loads env, initializes
  the database and migrations, runs startup cleanup, queues whatever should be building and has no build,
  starts the schedulers, then launches the API server and the repository file server.
- `backend/aurcache-api` owns the Rocket HTTP surface. Route registration is centralized in
  `src/backend.rs`, OpenAPI docs are assembled in `src/init.rs`, and the app serves docs at `/docs` and
  `/redoc`.
- `backend/aurcache-utils` holds most package/business logic. Package add/update flows resolve AUR or git
  sources, create package and dependency rows, and insert build rows (`ENQUEUED`, or `WAITING_FOR_DEPS`)
  that workers poll for and claim; the server never builds itself. Completion, publishing and promoting
  dependents live here too (`worker_complete`, `publish`).
- `backend/aurcache-db` owns SeaORM entities and migrations. `init_db()` applies migrations on startup;
  SQLite uses `./db` plus WAL pragmas, while Postgres is selected from env vars.
- `backend/aurcache-scheduler` contains background jobs such as auto-update, mirror ranking, and version
  checking.
- `backend/aurcache-cli` is a typed CLI for the HTTP API (`cargo run -p aurcache-cli -- ...`); the reusable
  Rust client library backing it lives in `backend/aurcache-client`. It authenticates via
  `Authorization: Bearer <token>`, resolving `--url`/`--token`, then `AURCACHE_URL`/`AURCACHE_TOKEN`, then
  `~/.config/aurcache-client/config.json`, then an interactive prompt (saved back to the config file). Use
  `--format json` for machine-readable output, or `raw` for endpoints without a dedicated subcommand.
  Onboarding commands run before the client is built, so they need no token: `repo config` prints the
  `pacman.conf` stanza (host arithmetic on the configured URL, in `url.rs`), and `completions` emits a
  shell script. `doctor` walks server → token → fleet → queue and exits non-zero on failure, presenting
  the `WaitingReason` the server already computes; `pkg add --from-installed` bulk-adds what
  `pacman -Qm` reports, behind a confirmation. `setup compose|server|worker` (in `compose.rs` and
  `setup.rs`) stands an instance up from nothing — a compose file for TrueNAS-style hosts, or
  `docker run` locally. The local server/worker pair shares an `enroll` volume so the worker
  self-approves, mirroring `compose/docker-compose.yaml`; compose output is templated rather than
  serialized so the explanatory comments survive. `repo config --install` appends to pacman.conf,
  falling back to `sudo` only on a permission error, and asks `GET /repo/info` how the repository is
  published when a token is configured (host substitution shared with the worker template through
  `aurcache_common::repo`).
- `backend/aurcache-common` is the shared leaf crate: the API types the server, CLI and browser frontend
  all speak, plus small helpers that would otherwise be duplicated or force a heavy dependency. Everything
  in it is dependency-free or behind a feature — `db` (sea-orm derives) and `fs` (filesystem helpers) are
  both on by default, and wasm consumers take `default-features = false` and still get the types. CI's
  "Check driver-free types" job (`cargo check -p aurcache-common --no-default-features`) is what keeps
  that true; a workspace build always turns the features on, so nothing else would catch a regression.
  It was called `aurcache-types` until it grew helpers as well as types.
- `frontend-rs/` is the Dioxus UI, in its own workspace because it only builds for
  `wasm32-unknown-unknown`. Routes are in `src/routes.rs`, the sidebar and layout in `src/shell.rs`,
  screens in `src/screens/`, and the pure filter/sort/paginate helpers in `src/listing.rs` — those are
  where list behaviour is tested, since components cannot be rendered in a unit test. The API client is
  `aurcache-client`, the same one the CLI uses, so API shapes are shared rather than re-declared.
- `docs/` is a separate Docusaurus site used for published documentation.

## Key conventions

- Keep changes in the owning crate instead of piling logic into `backend/aurcache`: HTTP schema/handlers
  belong in `aurcache-api`, persistence and migrations in `aurcache-db`, schedulers in
  `aurcache-scheduler`, and package, build and settings logic in `aurcache-utils`.
- Package addition is dependency-first. `aurcache_utils::package::add` recursively resolves AUR `depends`
  and `make_depends`, inserts dependency links, marks only the originally requested package as
  `directly_requested`, and initially enqueues only leaf packages.
- Successful builds fan out through the dependency graph. Once a build is published,
  `aurcache_utils::worker_complete::trigger_dependents` checks recorded dependency constraints and only
  promotes dependents when all dependency builds are ready and version-compatible.
- List columns are strings in the database but typed in the entities: `packages.platforms`
  (`Platforms`) and `build_flags` (`BuildFlags`) are `;`-joined, a worker's arches and affinity
  (`WorkerList`) `,`-joined, all in `aurcache_db::lists`; `source_data` and `split_packages` are JSON.
  Build and package statuses are `BuildState` (an integer column). Go through those types rather than
  joining or splitting by hand.
- A package's `status` is derived, never written directly: whatever changes a build's status calls
  `aurcache_db::helpers::builds::refresh_package_status` in the same transaction.
- Settings are read through `aurcache_utils::settings::get` with a typed `settings::key::*`, not by
  reading env vars ad hoc. The effective precedence in code is `Package -> Env -> Global -> Default`.
- API shapes live in `aurcache-common` and are shared, never re-declared. A field added to a response is
  added once and the CLI, the client library and the frontend all see it; mirroring a struct by hand is
  how the two ends drift apart.
- The frontend resolves the API against the origin the page was served from, falling back to
  `http://localhost:8080/api` when there is no window (see `frontend-rs/src/api.rs`).
- A size, a count or a total that is not known is `Option::None`, not `0`: "nothing recorded" and "zero
  bytes" are different answers, and the UI renders the first as a dash. Totals over several such values
  are all-or-nothing — a sum of only the known parts reads as a wrong number rather than as missing data.
- Every change to the pacman repository goes through `aurcache_utils::repository::Repository` (one
  instance, in `Services`): `repo.begin(&db)` takes its lock and recovers a pending update, the caller
  records additions and retirements, and `commit(&db, |txn| ...)` writes the new `repo.db`/`repo.files`
  beside the old, commits the caller's SQL changes and a recovery plan together (retried on failure),
  and only then moves staged files in and renames the databases over. The callback participates in
  that transaction; it never commits its own. Finalization errors retain the plan, and startup,
  the next update and `Repository::sweep` replay it before doing any more repository work. Blocking
  filesystem tasks retain the lock even when their awaiting future is cancelled. Private and risky
  first, the transaction next, renames last: a failure before the commit leaves the repository untouched. A retired file leaves `repo.db` at once but stays on disk,
  for clients holding an older `repo.db`, until `Repository::sweep` finds it unlisted for
  `RETIRED_PACKAGE_GRACE`; the sweep judges by `repo.db`, never by the `files` table. Never write `repo.db` or a repository file any other way -- the lock is what
  keeps two changes from each dropping the other's entry. Uploads are staged under the repository root
  (`staging_dir`), so publishing is a rename on one filesystem.
- Workers and the server agree on a numbered worker protocol at registration, never on versions:
  workers are released separately and may come from elsewhere. The types in
  `aurcache-common/src/worker.rs` are that protocol, and within one protocol they only gain optional
  fields; anything else -- a field required or changing meaning, a new enum variant a peer receives,
  a route removed, lease timing -- needs the next number in `PROTOCOLS`. The fixtures under
  `aurcache-common/tests/fixtures/worker-protocol/` fail on a break; never rewrite one to pass. The
  server keeps each agreement in memory and answers `428` to a worker it holds none for, which then
  registers again; a worker does no work before a registration succeeds. See
  `design/in-progress/worker-protocol-versioning.md`.
- A worker's successful completion is accepted at once: the build goes `ACTIVE` -> `PUBLISHING`, the
  lease ends, and `aurcache_utils::publish::publish_with_recovery` puts it in the repository in the background.
  Failures before the SQL commit fail the build; finalization failures preserve `PUBLISHING` and
  staging for recovery. The background task retries finalization and dependent promotion until they
  finish. A restart resumes `PUBLISHING` builds and successful builds whose staging
  directory still records pending dependent promotion. Lease policing (the
  heartbeat, the reaper, revoking a worker, claim capacity) only looks at `ACTIVE`, which is why
  publishing is a state of its own.
- Removing a package goes through `aurcache_utils::package::delete::package_delete`, never row by row.
  It is the only thing that does the whole job: the `builds`, `files`, `settings`, VCS-source and
  dependency rows, the artifacts and their `repo.db`/`repo.files` entries (through `Repository`), the
  build logs and the source checkout. It refuses outright while any package outside the set it is given
  still depends on one of them -- all of that goes together or none of it does -- so collecting a chain
  or a cycle passes the whole set at once, as `live_check` does. An orphaned `files` row is not a
  harmless leak: publishing reads it as "already produced by another package" unless its owner is gone.
- Migrations are upstream 0.5.0's seven, then `m20261008_000000_from_0_5`, which takes a 0.5.0
  database to the current schema in one step (it replaced the fork's own chain, which no public
  instance ran). A new migration goes after it. Read and write rows in a migration through explicit
  columns (sea-query, or a `FromQueryResult` of the columns needed), not through the entities: an
  entity has every column the code knows today, so a migration that loads one breaks as soon as a
  later migration adds a column -- which is how the old chain failed to upgrade a 0.5.0 Postgres
  database.
- The schema's foreign keys are enforced, on SQLite too — sqlx opens connections with `foreign_keys` on
  and `init.rs` sets it explicitly. `files.package_id` and both of `dependencies`' package columns
  cascade. A test that inserts a child row has to insert its package first.
- The repo includes multiple containerized workflows (all under `compose/`): `compose/docker-compose.hostmode.dev.yaml` mounts the host
  Docker socket for builds, `compose/docker-compose.dindmode.dev.yaml` is the simpler dev setup, and
  `scripts/test-e2e.sh` exercises `compose/docker-compose.e2e.yaml` end to end.

<!-- graft:start -->
## Graft — repo context graph

This repo is indexed in `graft/`: small linked markdown nodes that explain each
system and carry exact file:line spans, kept in sync with the code through git.

For ANY task here — understanding how something works, finding where code lives,
or scoping a change — get context from the graph before grepping or opening
source files. Re-ask freely (it's cheap) and reuse literal identifiers you
already have (symbol, error string, file name) as the query. New to this repo?
Run `graft map` first — a token-budgeted orientation (dir clusters, hubs,
hotspots), no LLM, no key.

- Run `graft ask "<your question>" --source` → ranked nodes with the relevant
  code spans inlined (each hit's ≤8-line crux by default; `--full` for whole
  definitions when the crux isn't enough). Match the tool to the task shape:
  for understanding or editing, the top node IS the answer — cite its
  `covers:` file:line spans and edit straight from `--source`. For
  exhaustive tasks ("every occurrence / every caller of this pattern"), ranked
  results are top-N, not complete — run `graft grep "<literal>"` instead
  (exhaustive over indexed files, grouped by enclosing symbol), falling back
  to raw `grep -rn` only for unindexed files.
- `graft skeleton <file>` → every definition's signature + span, ~10× cheaper
  than reading the file; use it to skim an API surface.
- `graft callers <symbol>` gives precomputed, exact edges — who calls this.
  Add `--direction out` for what it calls, or `--depth N` to walk
  transitively for the full blast radius. For structural questions, skip
  ranking and use this directly.
- Or browse: `graft/INDEX.md` lists every node; follow the links.
- Monorepos and folders of multiple repos rank fairly across sub-projects —
  hits carry `[scope/]` labels naming which one they're from. Narrow with
  `graft ask "<task>" --in <scope>/` once you know where you're working.

If a returned span is truncated ("+N more lines"), open the file at that exact
range before finalizing. Only open source files when a node genuinely lacks a
needed detail, and then at the exact file:line the node points to — never
re-read whole files.

After big code changes, refresh the graph with `graft build` (deterministic,
no API key, $0).
<!-- graft:end -->
