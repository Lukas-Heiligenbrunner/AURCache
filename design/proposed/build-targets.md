# Build targets: the same packages, built another way

Status: **Proposed** · Last updated: 2026-11-24

AURCache builds each package for each of its platforms (`x86_64`,
`aarch64`, `armv7h`), and a platform already means more than an
architecture: it decides `CARCH`, which official repositories the
dependencies come from (ALARM's for the ARM platforms), the mirrorlist, and
which workers can build it. This adds **build targets** that an instance
can define, so the same packages can be built another way without being
added again. The driving case is an `x86_64_v3` repository, built with
`-march=x86-64-v3`, beside the regular one.

A target is published like a platform, in a directory of its own.
Repositories choose the targets that they publish
(`multiple-repositories.md`), and packages only choose repositories. A
package is added once, keeps one dependency graph, and is built for every
target of its repositories.

---

## What else has this shape

The model has to fit more than v3, or it is a v3 feature with extra
parameters. These are the cases that it should cover, in the order in
which they would be done:

| Case | Differs in | Same base distribution | Published |
|---|---|---|---|
| CPU levels: `x86_64_v2`, `x86_64_v3`, `x86_64_v4`, `znver4`, a Pi 5 tuned aarch64 | `CFLAGS`, which workers can run the result | yes | yes |
| Toolchain tuning: LTO, PGO, `-O3`, clang | `makepkg.conf` | yes | yes |
| Another base distribution: Manjaro (which lags Arch), SteamOS, an Arch Linux Archive snapshot | official repositories and mirrorlist | no | yes |
| A canary against `core-testing`/`extra-testing`, which says that an upcoming Arch update breaks a package | `pacman.conf` | no | no |

The first two need only what is below. The last two need more, listed under
"Later".

## Where things stand

- **`Platform`** (`pacman-mirrors/src/platforms.rs`) is a closed enum of
  three. `packages.platforms` lists a package's platforms (default
  `x86_64`), and its dependencies inherit that list.
- **Per platform**, `aurcache-utils/src/job_config.rs` picks the official
  repositories (`official_repos`), writes `Architecture = <arch>` into the
  build's `pacman.conf` (`base_pacman_config`), and finds the mirrorlist
  (`mirrorlist_for`). The job carries the whole `makepkg_conf` and
  `pacman_conf`, so the server already decides the build's configuration.
- **Dependencies are per platform.** `trigger_dependents` promotes a
  dependent on one platform when its dependencies are satisfied on that
  platform (`dependencies_satisfied(db, deps, platform)`).
- **Workers report a list of architectures**, and claim only builds for
  those. They report nothing about the CPU they run on.
- **A worker shares one package cache per architecture** between builds,
  and reconciles it against AURCache's database for the job's `arch`
  (`aurcache-worker/src/job.rs`; see `stale-shared-pacman-cache.md`).
- **The repository** is `<root>/<arch>/<name>.db`. Clients and workers use
  `Server = …/$arch`, and `$arch` is the first value of pacman's
  `Architecture`.

## Design

### A target

A target has:

- a **name**, which is also its directory: `x86_64_v3`. It must not be a
  repository's name or `quarantine`, which are directories under the root
  too;
- a **base**, one of the built-in platforms. The base gives `CARCH`, the
  official repositories, the mirrorlist, the chroot and the architecture
  that a worker must report;
- a **`makepkg.conf` overlay**, applied after the global one and before the
  package's own: `CFLAGS="-march=x86-64-v3 …"`;
- the **CPU level** that a worker must have, if any (below).

The three platforms stay built in, as targets whose base is themselves. A
custom target is a row in a `build_targets` table, edited in the UI, and a
repository's platform list becomes a list of targets.

`CARCH` stays the base's. makepkg reads `source_<CARCH>`, `depends_<CARCH>`
and the other per-architecture arrays, so a `CARCH` of `x86_64_v3` would
silently drop the `x86_64` ones, and every `-bin` package would build
without its binary. A target's package files therefore have the same names
as the base's (`foo-1-1-x86_64.pkg.tar.zst`). They live in another
directory, and clients use one or the other, so that is no problem for the
repository. It is one for a worker's package cache (below).

### Which packages it builds

A target builds the packages of the repositories that publish it. Of
those, a target whose base is `x86_64` builds the ones whose PKGBUILD
allows `x86_64`, except those with `arch=(any)`, since every build of one
of those is identical. A v3 mirror of everything is the first repository
publishing `x86_64` and `x86_64_v3`. A v3 build of a few packages is a
second repository that lists them and publishes `x86_64_v3`.

A target's builds wait for their dependencies' builds on the same target,
as builds wait per platform today, except that an `any` dependency is
satisfied by its base build.

The version check, the dependency graph and the package's place in the UI
stay per package. A new version queues one build per target, as it queues
one per platform today.

### What a client sees

A client of `x86_64_v3` writes the target in place of `$arch`, and changes
nothing else:

```
[repo]
Server = http://nas.lan:8081/x86_64_v3
```

`Architecture` stays as it is: the packages are `x86_64` packages, and a
first architecture of `x86_64_v3` would point `[core]` and `[extra]` at
mirror directories that do not exist. The Repository page
(`repository-page.md`) prints this stanza for each published target.

**A target's database falls back to its base.** `<root>/x86_64_v3/repo.db`
lists, for each package, its newest successful `x86_64_v3` build, and the
`x86_64` build when there is no `x86_64_v3` build at all (an `any` package,
or one that has never built on v3) and the base is built for another
repository. The fallback files are hard links into the base directory, so
they take no space. An `any` package in a repository that publishes the
target but not its base is still built on the base, and its file is
published in the target's directory directly. So a client of the target
gets every package that the repository lists, as long as each has built
on the target or on its base. A failed v3 update behaves like any failed
build: the previous v3 build stays listed.

### Which workers build it

A `-march=x86-64-v3` build compiles anywhere, but its `check()` dies with
`SIGILL` on a CPU without those instructions, and so does anything that
the build runs. So a target can require a CPU level, and workers report
theirs: a `cpu_levels` field at registration (`x86-64-v2`, `x86-64-v3`,
`x86-64-v4`), read from what `ld.so --help` lists as supported. Only a
worker whose list includes the target's level can claim its builds. A
worker in a VM reports what the VM's CPU model exposes, which is what its
builds run on.

### What the job carries

The overlay travels in `makepkg_conf`, which the worker already applies.
Two things cannot travel in today's fields:

- **Where the build's dependencies come from.** The worker appends
  `repo_template`, with `Server = …/$arch`, which reads the base's
  directory. A target's build must read the target's directory, so that a
  dependency gets its v3 build.
- **Which package cache to use.** A target's packages have the same names
  as the base's, so sharing the base's cache would install one in place of
  the other. The worker also reconciles the cache against the database for
  `arch`, which does not list the target's packages.

So the job gains `repository: Option<JobRepository>`, with three fields:

- `name`, the section, since pacman reads `<section>.db`;
- `dir`, what replaces `$arch` in the `Server =` line;
- `cache`, the package cache to use, which the worker reconciles against
  that database.

The server picks one of the package's repositories that publishes the
build's target. An older worker would ignore the field and build against
the base, without any error. That changes the meaning of a job, so it is
the next worker protocol, and the server hands a target's builds only to
workers on that protocol. Builds that read the first repository on a
built-in platform do not change.
`cpu_levels` is a registration field, outside the protocol, so it needs
no bump.

### The UI

- A **Targets** section on the config files screen: each target's name,
  base, CPU level and `makepkg.conf` overlay.
- A repository's platform list offers targets.
- The package page and the build list show builds per target, as they show
  them per platform.

## Later

- **Another base distribution.** A target based on Manjaro needs its own
  official repositories and mirrorlist, so the base becomes a choice of
  sources rather than a built-in platform. Its dependencies have the same
  filenames as Arch's but different contents, which the per-target cache
  above already handles.
- **Targets that publish nothing.** A canary against the testing
  repositories produces a result, not a repository. Its builds would be
  checks: shown on the package page, and kept out of the package's status,
  so that a testing breakage does not mark the package failed.

## Open questions

- **CPU levels beyond x86-64.** `znver4` and aarch64 cores are not levels
  that `ld.so` lists. Would a list of CPU flags (from `/proc/cpuinfo`) be a
  better thing for workers to report?
- **Settings per target.** The overlay covers `makepkg.conf`. Should a
  target also be a settings scope, so that other build settings (the job
  timeout for an LTO target) can differ? Not needed for v3.

## Delivery

1. Targets in the database, with the three built-in ones, and a
   repository's platforms naming targets. This needs the first step of
   `multiple-repositories.md`, which moves platforms from packages to
   repositories. No visible change.
2. Custom targets on the base's distribution: the overlay, the fallback
   database, `cpu_levels`, the worker protocol with the job's repository,
   and the UI. This step covers v3 and toolchain tuning.
3. The cases under "Later", each with its own design when someone needs it.

## Alternatives

- **A separate repository per variant** (`[repo-v3]` before `[repo]` on
  the client). This needs no fallback database, but every client needs two
  stanzas in the right order, and the target's database needs another name,
  since a section reads `<section>.db`.
- **`CARCH=x86_64_v3`**, as some distributions name their v3 packages. File
  names would then differ, so workers could share one cache. But makepkg
  would skip every `_x86_64` array unless each PKGBUILD were rewritten.
- **A hard-coded `x86_64_v3` platform.** It is less code now, but it leaves
  out tuning, the other levels and other distributions, and each of those
  would then be another variant in an enum.
- **Build profiles attached to repositories.** Each repository would choose
  one profile, and builds would belong to profiles. A repository could then
  not fall back from v3 builds to regular ones, and it needs the same
  protocol change as targets do.
