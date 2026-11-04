# End-to-end tests as scenarios × topologies

`scripts/test-e2e.sh` began as one test: add a package, wait for it, install it.
Each variant since has been added by copying it. The hybrid scripts are thin
wrappers that set `E2E_COMPOSE_FILE`, `E2E_SERVICES` and
`E2E_EXPECT_DISK_USAGE`. `test-e2e-ssh.sh` is a fork with its own `dc`, `log`,
`cleanup`, readiness loops and status-code table. The next tests to write
(building the same package twice to exercise the kept build tree, a
dependency chain, a worker restart) would each be another fork. Most of their
lines would be the same as the others', and they would drift apart.

This design splits the three things those scripts mix together:

- **Topology**: how AURCache is deployed (split server and worker, hybrid
  image, legacy container builder). It is the harness's business.
- **Scenario**: what a user does to it and what must follow. It is a short
  bash script of `aurcli` commands.
- **Fixture**: an extra service a scenario needs (a git server behind SSH).
  It is an overlay, independent of the topology.

Any scenario can run on any topology that has what it needs, and adding a
scenario means adding one file.

Status: **Proposed** · Last updated: 2026-09-28

## Ground rules

- **A scenario is a list of `aurcli` commands.** Assertions are the CLI's exit
  status (`--wait`) or a `jq -e` over `--format json`.
  If an assertion cannot be written that way, the first fix is to make the CLI
  or API say it. The previous examples are `disk_usage` and `--wait`; the fact
  then serves users as well as the test. A scenario that greps container logs
  or runs `docker exec` into a worker is testing implementation details, and
  it breaks on the topologies where those details differ.
- **Scenarios know nothing about topology.** No compose file names, no service
  names, no ports. Anything a scenario has to branch on is a capability the
  topology declares (below), not a topology name.
- **Any number of workers.** A topology may run one worker or several, and
  every scenario has to mean the same thing on either. Nothing may assume
  "the worker": readiness waits for *all* of the topology's workers, helpers
  act on all of them unless told which, and a scenario whose assertion
  depends on where a build ran (a kept tree, a warm source cache) pins the
  package to one worker instead of hoping. An unpinned scenario that passes
  with one worker and flakes with two is a bug in the scenario. See
  [Several workers](#several-workers).
- **One stack per scenario.** Each scenario gets a fresh stack
  (`down -v`, `up`). Images are built once per topology per run, so the extra
  cost is startup and enrollment (tens of seconds), not a rebuild. Scenarios
  that share a stack would depend on each other's leftovers, and the reuse
  scenarios are exactly the ones where leftovers change the result.
- **The harness owns diagnostics.** When a scenario fails, the harness dumps
  the builds, the errors from the most recent unsuccessful build's log, and
  the service errors, the same way `dump_logs_on_failure` does now. It finds
  the failed build from `builds list --format json`, so scenarios never keep
  track of a `CURRENT_BUILD_REF`.
- **Never touch a real instance.** The developer's environment may point
  `aurcli` at production (`AURCACHE_URL` is exported in at least one
  maintainer's shell). The harness overrides `AURCACHE_URL` and
  `AURCACHE_TOKEN`, points `HOME` and `XDG_CONFIG_HOME` at a scratch
  directory so `~/.config/aurcache-client/config.json` cannot fill in the
  gaps, and runs the scenario with stdin from `/dev/null` so the interactive
  prompt cannot either.

## Layout

```
scripts/e2e/
  run.sh                        # the harness
  lib/                          # harness internals: stack.sh, diagnose.sh
  bin/                          # helpers put on a scenario's PATH
    aurcli -> backend/target/debug/aurcli
    e2e-install                 # pacman -S from the repo, in a throwaway container
    e2e-worker-env              # set host-level worker env, recreate, wait for the fleet
    e2e-expect-exit             # e2e-expect-exit <code> <cmd>...
  topologies/
    split/     compose.yaml  topology.env
    split-2/   compose.yaml  topology.env                 # two chroot workers
    hybrid/    compose.yaml  topology.env
    legacy/    compose.yaml  topology.env  setup.sh   # builds the builder image
  fixtures/
    gitssh/    compose.yaml  setup.sh                 # keypair into $E2E_SHARED
  scenarios/
    basic.sh
    ssh-credentials.sh
    rebuild-keeps-tree.sh
    ...
```

The e2e compose files move out of `compose/` into their topology or fixture
directories. They are test fixtures, not deployments, and each file should
sit next to the metadata that describes it. `compose/README.md` loses its e2e
table and points here instead.

`scripts/test-e2e.sh [package]` stays as a one-line wrapper,
`PACKAGE=… run.sh --topology split basic`, so the nightly workflow,
`AGENTS.md` and existing habits keep working. The two hybrid wrappers and
`test-e2e-ssh.sh` are deleted.

## Running

```bash
scripts/e2e/run.sh                                   # split × basic
scripts/e2e/run.sh rebuild-keeps-tree                # split × one scenario
scripts/e2e/run.sh --topology hybrid,legacy basic    # a column of the matrix
scripts/e2e/run.sh --topology all --all              # everything that applies
REUSE=1 scripts/e2e/run.sh basic                     # against the running stack
```

The harness runs each (topology, scenario) pair in turn and ends with a
summary table: pass, fail or skip, with the duration of each. `CLEANUP`,
`REUSE` and `VERBOSE` mean what they mean today. On a failure the stack is
kept, and the run stops unless `--keep-going` is given. The kept stack is the
one to debug, and the next pair would tear it down.

## The topology contract

A topology is a directory. `topology.env` is sourced by the harness:

```bash
# topologies/split/topology.env
E2E_SERVICES="aurcache builder"   # must stay alive for the run to mean anything
E2E_WORKERS="builder"             # worker services; each is also its WORKER_NAME
E2E_CAPS="chroot disk-usage kept-tree"

# topologies/split-2/topology.env
E2E_SERVICES="aurcache builder-a builder-b"
E2E_WORKERS="builder-a builder-b"
E2E_CAPS="chroot disk-usage kept-tree multi-worker"

# topologies/hybrid/topology.env -- the worker runs inside the server container
E2E_SERVICES="aurcache"
E2E_WORKERS="aurcache"
E2E_CAPS="chroot disk-usage kept-tree"
```

`E2E_WORKERS` lists the compose services that run a worker. Its length is how
many approved, online workers the harness waits for, and a service may be in
both lists, as in `hybrid`.

`setup.sh`, if present, runs once per run before the images are built. For
`legacy`, it builds `aurcache-legacy-builder:test` and creates the host
directory that the Docker daemon binds.

Every topology's `compose.yaml` must:

- serve the API on `${AURCACHE_PORT}` and the repository as
  `http://aurcache:8081` on the network `aurcache_network`. `e2e-install` and
  the fixtures join that network by name;
- have workers that enroll themselves with no manual step;
- set each worker's `WORKER_NAME` to its service name. This is the only link
  between a compose service and the worker the API reports, and the harness
  uses it to find the logs of the worker that ran a failed build;
- give each worker its own data volume. Workers that share one would share an
  identity and a pool, which is not a topology anyone deploys;
- give each worker service
  `env_file: [${E2E_ENV_DIR}/all.env, ${E2E_ENV_DIR}/<service>.env]` and mount
  `${E2E_SHARED}:/e2e:ro`. These two hooks let a scenario change host-level
  worker settings without knowing which services are workers.

Two workers mean two service blocks that differ only in name and volume.
A YAML anchor keeps them from drifting. `docker compose --scale` does not
work here, because scaled replicas share volumes and cannot have separate
names.

### Readiness

The stack is ready when the API answers and `aurcli worker list` shows every
name in `E2E_WORKERS` as approved and online. "At least one approved" is not
enough, because a scenario would then start with part of the fleet and pass
or fail depending on which worker enrolled first. The same wait runs after
`e2e-worker-env` recreates workers. Workers keep their identity across a
recreate, since their data volume survives, so this means waiting for the
same names to come back, not for new enrollments.

**Capabilities** are the only topology facts a scenario sees. Today that
covers `disk-usage` (which replaces `E2E_EXPECT_DISK_USAGE`), `chroot`, and
`kept-tree` (the builder can keep a build tree between builds), and
`multi-worker` (more than one worker, for scenarios about routing). A scenario
declares the capabilities it needs in its header, and the harness skips it,
with the reason, on topologies that lack them. A scenario can also read
`$E2E_CAPS` to make one assertion conditional. That is fine for a single
line, but anything larger belongs in a separate scenario.

## The scenario contract

```bash
#!/usr/bin/env bash
# A package built twice with a kept tree reuses it the second time.
# requires: kept-tree
# fixtures:
# timeout: 1200

# The tree lives on one worker, so both builds must run there.
worker=$(e2e-pick-worker)
aurcli worker config "$worker" --set packages=hello

aurcli pkg add hello --platform x86_64 --persist-build-dir --wait
aurcli pkg update hello --force --wait
aurcli --format json builds get hello/2 \
    | jq -e --arg w "$worker" '.worker_name == $w and .build_tree_reused'
e2e-install hello
```

- The harness runs it as `bash -euo pipefail <scenario>` in a scratch working
  directory, so the first failing command fails the scenario. Exit 0 is a
  pass, exit 77 is a skip decided at run time (the automake convention), and
  anything else is a failure.
- Its environment is `AURCACHE_URL`, `AURCACHE_TOKEN`, the isolated `HOME`,
  `E2E_CAPS`, `E2E_SHARED`, and `PATH` with `bin/` first.
- The header lines are parsed by the harness. `requires:` filters topologies,
  `fixtures:` adds overlays, and `timeout:` bounds the whole scenario. The
  CLI's own `--wait-timeout` bounds each wait.
- Arguments come from the environment with defaults, like
  `PACKAGE="${PACKAGE:-hello}"` in `basic.sh`, not from positional parameters.
  The harness cannot know what they mean.

### Helpers

The helpers are executables on `PATH`, not shell functions. They work the same
way whether a scenario is run by the harness or pasted into a terminal
against a kept stack.

- `e2e-install <pkg>... [-- <cmd>]` installs the packages, from the
  repository, in a throwaway `archlinux` container on the stack's network,
  then runs `<cmd>` there if one is given. This is today's `validate`, plus
  the SSH test's marker check.
- `e2e-pick-worker` prints the name of one approved worker, the first in
  `E2E_WORKERS`. It is a helper rather than `aurcli worker list | jq '.[0]'`
  so the choice is deterministic and stays the same when a scenario is re-run
  by hand.
- `e2e-worker-env [--worker NAME]... VAR=value...` sets **host-level**
  settings, the ones the server may not set (`WORKER_GIT_SSH_KEY`,
  `WORKER_BIND_MOUNTS`, and so on). Without `--worker` it writes `all.env`
  and recreates every worker; with it, it writes only those workers' files.
  Then it waits for the whole fleet as in [Readiness](#readiness). This is
  the only topology-aware thing a scenario can ask for.

Worker **policy** (affinity, concurrency, priority, limits) does not go
through a helper. `aurcli worker config <worker> --set key=value` changes it
through the server at runtime, the same way an operator would. It needs no
restart and does not depend on the topology, so scenarios use it directly.
Choosing between the two follows the split in
`design/implemented/worker-configuration.md`: if the server refuses the
setting, it is a host setting.
- `e2e-expect-exit <code> <cmd>...` passes only if the command exits with that
  exact code. A "this must fail" phase needs it, because a bare `!` would also
  accept a timeout or a crash.

`docker compose` is deliberately not exposed. If a scenario needs something
the helpers do not offer, the helper set is what grows.

## Fixtures

A fixture is a compose overlay plus an optional `setup.sh`, applied when a
scenario lists it. It may only add services on `aurcache_network`. It must not
modify topology services, because it would have to name them, and those names
differ across topologies. Whatever a fixture has to give the worker goes
through `$E2E_SHARED`, and the scenario decides whether the worker gets it.

The SSH test shows how this works:

```bash
# scenarios/ssh-credentials.sh
# Build credentials reach the unprivileged makepkg user inside the chroot.
# requires: chroot
# fixtures: gitssh

# Unauthorised first: the fixture must genuinely need the key, or the
# authorised phase could pass for the wrong reason.
e2e-expect-exit "$AURCLI_EXIT_BUILD_FAILED" \
    aurcli pkg add git://gitssh/pkg.git --ref master --wait
e2e-worker-env WORKER_GIT_SSH_KEY=/e2e/gitssh/id_ed25519
aurcli builds retry pkg/1 --wait
# the marker the fixture baked into the source
e2e-install pkg -- grep -q "$MARKER" /usr/share/pkg/marker
```

The phases no longer need a full `down -v`/`up` between them. Only the
workers are recreated. The key goes to every worker, so the retry succeeds
whichever one claims it. With several workers, the failing phase is also
stronger: no worker may have been able to fetch the source.

## Several workers

Most scenarios do not care which worker builds what, and they run unchanged
on `split-2`. Running `basic` and `dependency-chain` there is itself a test:
a chain whose links land on different workers has to fetch its dependencies
from the repository, not from a local cache.

A scenario that does care says so in one of two ways:

- **Pin with affinity.** `aurcli worker config <w> --set packages=<pkgbase>`
  reserves the package for that worker: once an approved worker claims it,
  no other worker may build it (`design/implemented/worker-routing.md`). This
  is how `rebuild-keeps-tree` makes sure both builds run where the tree is.
  It then checks `worker_name` on the build, so a routing regression shows up
  as a clear mismatch instead of a missing reuse.
- **Require `multi-worker`** when the scenario is about distribution itself.
  For example, affinity keeps a package on its worker while the other one
  sits idle, or pausing a worker (`aurcli worker pause`) moves new builds to
  the rest of the fleet.

On a failure, the harness reads `worker_name` from the failed build and shows
that service's log, not every worker's.

## What the CLI needs

The rule is that a scenario is only `aurcli`, `jq` and the `e2e-*` helpers.
So whatever a scenario needs to do or check, the CLI (and the API behind it)
has to cover first. Each gap is also a gap for an operator. This is the list
the initial scenarios need, checked against the current CLI.

**Already covered:**

- `pkg add` / `pkg update` / `builds retry` with `--wait`,
  `--wait-timeout` and `--wait-stall-after`;
- `builds list --worker --status --package` and `builds get` with
  `worker_name` and `disk_usage` in its JSON;
- `worker list` with `name`, `status`, `online`, `paused` and
  `package_affinity`;
- `worker config <id> --set` and `--reset` for policy, `worker pause` and
  `resume`.

**Missing:**

- **Distinct exit codes for `--wait`**: one for "a build failed", another for
  "the wait gave up" (timeout, stall). They get documented constants
  that the harness exports as `AURCLI_EXIT_*`. Without them, an expected
  failure cannot be told apart from a hung queue.
- **Workers addressed by name.** `worker config`, `pause`, `resume` and
  `revoke` take an id. `builds list --worker` already takes an id or a name,
  and the others should too. Names are what the topology controls, and ids
  depend on enrollment order, which differs between runs with two workers.
- **`--persist-build-dir` on `pkg add` and `pkg patch`.** Today `pkg patch`
  has only `--platform`, `--build-flag` and `--patch`, so a scenario could
  only set it through `aurcli raw`.
- **Whether a build reused its kept tree.** `disk_usage.build_tree` only says
  a tree exists, not that the build started from it. A field on the build
  (shown on the build page too) answers the question the scenario asks. The
  same goes for a warm source cache, if a scenario ends up asserting that.
- **When a worker's base chroot was last refreshed, and how it went**
  (swapped in, or failed and kept the old base), on `worker list`. Without it
  `base-refresh` can only show that the second build succeeded, not that a
  new base was swapped in first -- and an operator asking why builds see a
  stale base has the same question.
- **Waiting for the fleet**: `worker list --wait-online <n|names>`, or a
  `doctor` mode that waits instead of reporting. The harness's readiness loop
  and `e2e-worker-env` would both use it instead of polling with `jq`, and an
  operator bringing up a worker has the same question.

A scenario that needs something not on this list is a prompt to add it here
first, not to reach for `aurcli raw` or `docker exec`.

## Initial scenarios

| Scenario | Requires | Topologies in CI |
|---|---|---|
| `basic`: add, wait, install | — | split nightly; hybrid, legacy, split-2 with `run-e2e` |
| `ssh-credentials` | chroot, fixture `gitssh` | split nightly |
| `rebuild-keeps-tree`: build twice on a pinned worker, the second reuses the tree | kept-tree | split, split-2 nightly |
| `base-refresh`: with `chroot_refresh_interval=0`, build twice on a pinned worker; the second refresh swaps a new base in and the build succeeds | chroot | split nightly |
| `kept-failure`: with `keep_failed` set, a `--patch`ed PKGBUILD whose `build()` fails, `--persist-build-dir`; the build fails, `builds get` reports `.kept.path` and `.kept.tree`; the fixed PKGBUILD then builds cold | chroot, kept-tree | split nightly |
| `dependency-chain`: a package with AUR deps, built leaf first | — | split-2 nightly |
| `worker-restart`: restart between two builds; enrollment and cache survive | chroot | split nightly |
| `affinity`: a pinned package only ever builds on its worker | multi-worker | split-2 nightly |
| `pause-drains`: a paused worker gets no new builds; the others take them | multi-worker | split-2 nightly |

## Migration

1. Extract `lib/`, `run.sh`, the `split` topology and `basic`. Make
   `test-e2e.sh` the wrapper. The nightly job must still pass unchanged.
2. Add the `hybrid` and `legacy` topologies. Delete their wrapper scripts and
   update `AGENTS.md`'s "Which end-to-end suite to run".
3. Close the CLI gaps above, which are independent of the harness and can
   land in any order. Add `e2e-worker-env` and the `gitssh` fixture, port the
   SSH test, and delete `test-e2e-ssh.sh`.
4. Add `split-2` and run the existing scenarios on it. Whatever turns out to
   assume a single worker gets fixed there, before new scenarios copy the
   assumption.
5. Add the new scenarios.

## When this stops being bash

The harness is lifecycle management (compose, polling, log dumping), and
bash is good at that. Scenarios stay bash as long as they are sequences of
commands. Moving to a Rust harness (an `E2eStack` fixture using
`aurcache-client`, with scenarios as opt-in `#[test]`s) becomes worth it when
any of these happens:

- a scenario needs concurrency or fault injection at a precise moment (kill
  the worker mid-build, two builds racing for one package);
- assertions need to compare state across builds beyond a `jq` one-liner;
- we want to run pairs in parallel. That needs per-pair compose project names,
  port allocation, and the legacy builder's fixed host directory solved first.

Scenarios written as `aurcli` commands port mechanically when that time comes.

## Open questions

- **Heterogeneous fleets.** `split-2` has two identical x86_64 workers. A
  fleet with an emulated aarch64 worker would test arch routing, but a
  capability like `arch:aarch64` needs binfmt on the CI runner. Leave it out
  until a scenario needs it.
- **Multiple topology files vs. parameters.** `split` and `legacy` are two
  axes (layout and builder) presented as a single name. Three topologies do
  not justify a matrix of axes yet, and a fourth might. Revisit then.
- **Where the e2e compose files live.** Moving them breaks muscle memory for
  anyone running `docker compose -f compose/docker-compose.e2e.yaml` by hand.
  The alternative is keeping them in `compose/` and having `topology.env`
  point at them.
