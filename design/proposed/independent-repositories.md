# Independent repositories: one instance or several

Status: **Proposed** · Last updated: 2026-11-24

Some setups want repositories that differ in both their packages and how
they are built: a `work` repository of a few tools built for `x86_64_v3`
with LTO for the office machines, beside a home `repo` of everything,
built generically. Today that takes two AURCache instances, each with its
own workers, settings, UI, upgrades and backups. The goal is to make such
repositories simple to run.

There are two ways to do it, compared below:

- **One instance, independent repositories.** A repository chooses its
  packages and the targets that they are built for. This is
  `multiple-repositories.md` (which packages) and `build-targets.md` (how
  they are built) together.
- **A fleet.** One instance per repository, as today, plus the pieces that
  make several instances feel like one: the CLI, an overview, and one
  address for the repositories.

---

## Where things stand

- **One instance publishes one repository**, under `<root>/<arch>/`, and
  every package is built the same way (per platform).
- **One client, one server.** `aurcli` and `aurcache-client` hold one URL
  and one token (`ClientConfig`, `aurcache-cli/src/config.rs`).
  `tui-client.md` lists "several instances at once" as out of scope.
- **The web UI talks to the server that serves it**
  (`frontend-rs/src/api.rs`), and the API has no CORS configuration.
- **A worker enrolls with one server.**

## One instance

A package lists only its repositories, and each repository lists the
**targets** that it publishes. A package is built for every target of its
repositories. So:

- `repo` lists everything, on `x86_64`;
- `work` lists five packages, on a target `x86_64_v3_lto` (base `x86_64`,
  `-march=x86-64-v3 -flto`, workers with v3);
- a package in both is built twice, once per target. A package only in
  `work` is built only for `x86_64_v3_lto`.

All of it comes from the other two designs: repositories, membership,
platforms chosen per repository, dependencies following a package into a
repository, publishing with hard links, and builds reading their own
repository (its third step) from `multiple-repositories.md`; the
`makepkg.conf` overlay, CPU levels for workers, the job's repository and
the fallback database from `build-targets.md`. Builds reading their own
repository is what makes `work` possible: a dependency that is only in
`work` is only built for `x86_64_v3_lto`, so a build can only install it
from `work`. Independent repositories need no code beyond those two.

What it gives:

- **One worker pool.** Every worker can take builds for any repository, as
  far as its architecture and CPU allow. With two instances, each needs its
  own workers, or one machine runs a worker for each and they compete
  blind.
- **Shared work where it overlaps.** A dependency that both repositories
  need on the same target is built once. Separate instances build it twice.
- **One place for everything**: one UI, one API token, one upgrade, one
  backup, one set of global settings.

What it costs:

- **Every repository shares one failure domain.** One database, one queue,
  one server: a bad upgrade or a full disk stops all of them.
- **No per-repository access.** Anyone who can manage the instance can
  manage every repository. `api-tokens.md`'s scopes would need a
  repository dimension to change that.
- **Settings stay global.** The version check interval and the auto-update
  schedule apply to every repository, and a target overrides only
  `makepkg.conf`.
- **One name per package.** A repository cannot have its own fork of `foo`.

## A fleet

Each repository is its own instance. What is needed is the layer on top:

- **Named instances in the CLI** (`aurcli --instance work pkg list`), with a
  default. This is cheap, and useful by itself.
- **An overview** that lists the instances with what each dashboard shows
  (queue, failures, out-of-date packages, workers waiting for approval) and
  links into each instance's own UI. Either the web UI calls other
  instances (which needs CORS, and tokens for other origins in the
  browser), or a small separate frontend with a backend holds the tokens.
- **One address for the repositories**: an nginx template that serves
  `https://repo.example.com/<instance>/$arch` from each instance, with each
  instance's `AURCACHE_PUBLIC_URL` set to its path.
- **Configuration kept in step**, if wanted: copying global settings and
  config files from one instance to another, through the dump format
  (`db-export.md`).

What it gives:

- **Isolation.** Instances fail, upgrade and are restored separately, and
  can be run by different people.
- **Location.** An instance can sit next to the machines that use it, or on
  the hardware that suits its builds: a native aarch64 box, a VPS for
  machines outside the LAN.
- **No change to AURCache's model.** Every piece is outside the server.

What it costs:

- **Workers per instance.** Nothing schedules across instances, so a
  machine's time is split by hand. A worker that registers with several
  servers is allowed by the protocol, but each server sees it as fully
  available.
- **Duplicate builds** of whatever the repositories have in common.
- **N of everything else**: upgrades, backups, global settings, signing
  keys (each client trusts one key per instance).
- **The overview is new software** to maintain, with its own question of
  where tokens live.

## Comparison

| | One instance | Fleet |
|---|---|---|
| Builds shared where repositories overlap | yes | no |
| Workers shared | yes, scheduled | by hand |
| Upgrades, backups, keys | one | one per instance |
| A failure stops | every repository | one repository |
| Per-repository access | not without token scopes | yes |
| Different locations | no | yes |
| New code | none beyond the two designs | CLI instances, overview, nginx template |
| Also useful alone | each of the two designs | CLI instances |

## Recommendation

One instance, because it is the other two designs together. Those two
are worth building for their own cases anyway, and independent
repositories then need nothing more. A fleet's advantages are isolation,
location and separate ownership. Those are reasons to run separate
instances whatever AURCache supports, and they need only the CLI's named
instances and a documented proxy, which are cheap enough to do in any
case.

## Open questions

- **Per-repository schedules.** Is a global version check and auto-update
  schedule acceptable for independent repositories, or does a repository
  need to be a settings scope too?
- **Per-repository access.** Is it a requirement, which would favour the
  fleet until tokens have scopes?
