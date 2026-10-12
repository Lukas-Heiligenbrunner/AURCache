# Rebuilding packages when what they were built against changes

Status: **Proposed** · Last updated: 2026-10-12

`eternalterminal` links `libprotobuf.so.36.1.0`. When `protobuf` moves to 37
in `extra`, that file is gone and the published `eternalterminal` stops
starting — on every client that upgrades, until someone rebuilds it. AURCache
already rebuilds on a new upstream version; this doc covers rebuilding on a
change *underneath* a package: a shared library's soname, an interpreter's
versioned module directory, and the cases only a person can name.

Two constraints shape it:

- **As little custom ELF parsing as possible.** Library resolution is `ldd`'s
  job, run where the libraries are installed; the interpreter rules are
  `rebuild-detector`'s, adopted as data.
- **A per-package "rebuild on X" setting only where no blanket rule is both
  safe and cheap.** The blanket rules below fire only on a real break; the
  setting covers what they cannot see.

---

## Where things stand

- Rebuilds have two triggers today, neither of which sees this:
  `check_versions` (`aurcache-scheduler/src/update_version_check.rs:52`)
  compares upstream versions, and `trigger_dependents`
  (`aurcache-utils/src/worker_complete.rs:231`) promotes dependents on
  recorded *version constraints* between packages AURCache builds itself.
- A dependency satisfied by the official repositories leaves no record: it
  resolves through `OfficialRepos::holds` (`aurcache-deps/src/repo.rs:100`),
  which answers yes or no from a set of names. `names_in_desc` (`:365`) keeps
  `%NAME%` and `%PROVIDES%` and drops every version. The databases are
  refreshed hourly, `x86_64` only (`:23`).
- A rebuild at an unchanged version republishes **the same filename** with
  new contents (`aurcache-utils/src/publish.rs:484`). pacman does not upgrade
  a package whose version has not moved, so such a rebuild never reaches a
  client that already has the package — and a client holding the old file in
  its cache sees a checksum mismatch.
- Per-package source patches exist: `SourcePatch` and
  `apply_patch_to_archive` (`aurcache-utils/src/snapshot.rs:975`) rewrite
  files in the source snapshot and regenerate `.SRCINFO`.
- Nothing reads `.BUILDINFO`. Every package carries one: the full list of
  packages and versions installed in the chroot that built it.

### What makepkg and Arch already provide

- `makepkg`'s `autodeps` option (`/usr/share/makepkg/autodep/library_depends.sh`)
  does not help. It is off in the stock `OPTIONS`, and it only adds a
  dependency on `lib:<soname>` when something installed *provides*
  `lib:<soname>`. Arch's packages provide sonames without that prefix
  (`protobuf`: `libprotobuf.so=36.1.0-64`), so the check never matches.
- Most official libraries do provide their sonames, unprefixed:
  `openssl` (`libssl.so=3-64`), `icu`, `boost-libs`, `libxml2`, `protobuf`.
  Some do not: `glibc`, `qt6-base`, `abseil-cpp` — and `abseil-cpp`'s
  date-versioned sonames are exactly what a `protobuf` user also links.
- `rebuild-detector` (`checkrebuild`, in `extra`) is a 150-line script that
  finds packages needing a rebuild on a *live system*:
  - **ldd**: every ELF file of the checked packages through `ldd`, flagging
    any `not found`;
  - **python**: packages owning `/usr/lib/python3.X` for an X other than the
    running interpreter's;
  - **perl**: `/usr/lib/perl*/<v>` other than the running `perl`'s;
  - **ruby**: `<gem dir>/<v>` other than `gem environment gemdir`'s;
  - **haskell**: `/usr/lib/ghc-<v>` other than `ghc --numeric-version`.

  It works on installed packages, so it cannot run on the server as-is. Its
  rules carry over directly, though, and its ldd check is the model for what
  a worker records.
- The official `.files` databases list every package's files: `core.files`
  is 1.5 MB, `extra.files` 49 MB (against 8.5 MB for `extra.db`).

---

## Proposal

**Record at build time, on the worker, what each artifact links; decide on
the server, from the repository databases, whether that still exists; queue
what does not, with a version bump so clients pick it up.**

Three checks, by how they learn what a package needs:

| Check | Learns from | Decided against | Custom parsing |
|---|---|---|---|
| Libraries | `ldd`, in the build chroot | soname `provides`, else `.files` | ldd's text output |
| Interpreters | the artifact's file list (`repo.files`) | the interpreter's version in the sync DB | a path prefix |
| Named packages (setting) | the artifact's `.BUILDINFO` | that package's version now | `key = value` lines |

None reads an ELF file.

### 1. Libraries: `ldd` on the worker

After `makepkg` succeeds, still inside the build chroot (where the
dependencies the binaries link are installed) and as the build user:

1. List each produced package's regular files (`bsdtar -tf`), extract them
   to a scratch directory, and run `ldd` on every executable or `.so` file.
   Non-ELF files make `ldd` print "not a dynamic executable"; those are
   skipped, so no `file` pre-filter is needed.
2. Keep each `soname => /usr/lib/... (0x…)` line.
3. Drop sonames the package ships itself (the soname appears in its own file
   list) and `not found` lines. A `not found` at build time is a prebuilt
   binary missing an optional library, not a break this check can repair —
   `rebuild-detector` would flag it forever.
4. Map each resolved path to its owner with one `pacman -Qqo` over the set.

The worker reports the result in `CompleteReport`, per produced package:

```rust
/// A shared library an artifact resolved at build time, and who provided it.
pub struct LinkedLibrary {
    pub pkgname: String,
    pub soname: String,   // libprotobuf.so.36.1.0
    pub path: String,     // /usr/lib/libprotobuf.so.36.1.0
    pub owner: String,    // protobuf
}
```

`ldd` may execute part of the dynamic loader on its argument, which is why it
runs as the build user, in the chroot, under the build's limits: the same
code already ran there with the same rights during `makepkg`, so this adds no
trust. On the server, `publish` stores the list in a new `linked_libraries`
table keyed by `build_id`, in the same transaction as the build's files.

**Deciding**, after each refresh of the official databases that downloaded
something, and after each publish of our own:

- An `owner` whose version is unchanged is not looked at.
- For an `owner` whose version changed, the soname is still provided if a
  package in the current repositories provides `<name>.so=<version>-64` for
  it (`libprotobuf.so.36.1.0` ↔ `libprotobuf.so=36.1.0-64`). That needs
  `OfficialRepos` to keep `%VERSION%` and versioned `%PROVIDES%`, not just
  names. Almost every soname bump is decided here, from `extra.db`.
- An owner that provides no sonames (`glibc`, `qt6-base`, `abseil-cpp`) is
  decided by the path: does any current package still contain it? The
  official `.files` databases answer that. They are fetched only when such an
  owner changes, with the same TTL as the `.db` files.
- An owner that is one of our own packages is decided against `repo.files`,
  which `Repository` already writes. This covers an AUR library of ours
  bumping its soname, which `trigger_dependents` misses because the version
  constraint is still satisfied.

A package with a library that is no longer provided is **stale**.

**What this cannot see: an ABI change without a soname change.**
rebuild-detector's open issue max-baz/rebuild-detector#39 is exactly the
`protobuf` case. `icinga2` fails with an undefined symbol
`…absl::lts_20260107::Cord…`: `protobuf` was rebuilt against a new
`abseil-cpp`, whose inline namespace changed its symbols. `protobuf`'s soname
did not move, so every library still resolves and `ldd` (even `ldd -r`)
reports nothing. A soname check, ours or rebuild-detector's, cannot catch
this. Two answers, neither blanket:

- `rebuild_on` (below): `eternalterminal` names `protobuf abseil-cpp`.
- A symbol check, i.e. whether every undefined dynamic symbol is still
  defined by the library that resolved it. That needs the *new* libraries
  installed next to the binary, so it belongs in a verification chroot, not
  on the server. It is a candidate for the shared work below, since #39 is
  upstream's problem too.

### 2. Interpreters: `rebuild-detector`'s rules, on `repo.files`

This check is stateless. A rule is a path pattern and an interpreter package
whose version decides it:

| Rule | Path in the artifact | Current from | Compared | Exempt owners |
|---|---|---|---|---|
| python | `usr/lib/python3.X/` | `python` | `3.X` vs major.minor | `python3[0-9]*` |
| perl | `usr/lib/perl*/5.X/` | `perl` | `5.X` vs major.minor | |
| ruby | `usr/lib/ruby/gems/X.Y.0/` | `ruby` | `X.Y` vs major.minor | |
| haskell | `usr/lib/ghc-X.Y.Z/` | `ghc` | full pkgver | |

These are `checkrebuild`'s four rules with the live interpreter replaced by
the sync DB's version of it. Even this short list has a detail that is easy
to drop when copying it: the python rule exempts the packages named
`python3[0-9]*`, the parallel interpreters like `python311` that own their
own `usr/lib/python3.11/` legitimately. The rules should not be maintained
as a second copy; see [Sharing the rules with rebuild-detector](#sharing-the-rules-with-rebuild-detector).

The table is data and is tested rule by rule, so a fifth
(`usr/lib/lua/5.X/`, kernel modules under `usr/lib/modules/<kver>/` for
packages that ship prebuilt ones) is one row. On each refresh, every
published package whose `repo.files` entry has a path under a version other
than the current one is stale.

### 3. Named packages: the `rebuild_on` setting

The blanket checks fire only on something provably gone. Some breaks leave
nothing gone:

- **Qt private API.** A package built against `qt6-base` 6.9's private
  headers breaks on 6.10 with the soname still `libQt6Core.so.6`. Arch pins
  these with exact-version dependencies.
- **Haskell and OCaml.** The ABI is a hash that changes on almost every
  library update. A blanket rule would rebuild everything.
- **Node native addons, R packages.** These are built for one ABI with no
  versioned path that `repo.files` could show.
- **Static linking.** A Rust or Go binary does not break, but it does not
  get a toolchain's or a library's security fix until it is rebuilt.

None of these is safe as a blanket policy. A false negative is a broken
package, and a broad enough rule to catch them rebuilds everything weekly.
So they are per-package and opt-in:

- `rebuild_on`: `Scope::Package` (a server-wide value a package overrides,
  empty by default), a list of package names, e.g. `qt6-base` or
  `rust go`.
- A package is stale when a named package's **pkgver** in the current
  repositories (official or ours) differs from the version in the
  artifact's `.BUILDINFO`. `.BUILDINFO` is read from the published artifact
  when the check needs it rather than stored: few packages set the setting,
  and the file already sits in the repository.
- A name absent from `.BUILDINFO` (a typo, or a package that was not in the
  chroot) is reported on the package page, not ignored.

The setting answers the question of when to rebuild, not how to find out
that a rebuild is needed. It is not the mechanism for sonames. Those are
caught for every package without configuration.

### 4. Queueing, and making the rebuild reach clients

A stale package is queued like any rebuild, with a recorded reason
(`libprotobuf.so.36.1.0 no longer provided`, `python 3.13 → 3.14`,
`qt6-base 6.9.2 → 6.10.0`), shown on the build and in the activity log:

- **Gated** on a global `rebuild_on_abi_break` (default on, as
  `build_on_new_version` is) and, like `build_on_new_version`, only for
  packages whose last build succeeded. A failing package waits for a person.
- **In one pass.** Every stale package found by one check is queued
  together, so a Python bump that breaks thirty interdependent packages goes
  through the existing dependency-first ordering (`WAITING_FOR_DEPS`)
  rather than thirty independent builds racing.
- **Once per trigger.** A rebuild records the trigger it answers (owner and
  the version that caused it). If the new build still fails the same check
  for the same trigger, the package is not queued again. Instead it is shown
  as "rebuilt but still unresolved" until a newer trigger arrives. This is
  what stops a worker whose mirror lags the server from relinking the old
  library in a loop. It also catches a package that genuinely cannot build
  against the new version and still links the old one through a bundled
  copy.

**The version bump.** A rebuild has to change the version, or pacman never
installs it. AURCache tracks a per-package `rebuild` counter `n`, reset to 0
whenever upstream's `pkgver-pkgrel` changes, and increments it for each of
these rebuilds. The source snapshot handed to the worker gets a generated
`SourcePatch` setting `pkgrel=<upstream pkgrel>.<n>`:

- `vercmp` orders it correctly: `1-1 < 1-1.1 < 1-1.2 < 1-2`. Upstream's next
  real release wins as it should.
- makepkg allows a pkgrel of the form `integer[.integer]` and nothing more.
  A PKGBUILD whose own pkgrel is already fractional (`1.1`) cannot take a
  suffix. Such a package is flagged as needing a manual rebuild rather than
  given a version that could collide with upstream's next one.
- Version comparisons strip the suffix: `check_versions` compares upstream's
  `1.0-1` against the stored `1.0-1.1` and must not see a new version, and
  publishing records the full version for the build but the base version for
  `upstream_version`.

---

## Sharing the rules with rebuild-detector

What breaks a package is not AURCache's knowledge; it is Arch's, and
rebuild-detector is where it is kept. A copy in AURCache would drift: every
fix upstream (#40/#41, a localized `stat` silently disabling the ldd check,
merged 2026-09-28) or new rule would have to be noticed and ported by hand,
and the copy would quietly fall behind. The better outcome is
rebuild-detector split into parts both of us use, with AURCache contributing
the split.

### Where upstream stands

- `max-baz/rebuild-detector` (moved from `maximbaz/`) is maintained: the
  last merge was 2026-09-28. It is one bash script, `checkrebuild`, plus a
  pacman hook, with no tests.
- The rules are code: one `get_broken_<kind>_pkgs` function each, every one
  of which asks the live system (`pacman -Qqo`, `python3 -c …`, `gem
  environment`) for both the facts and the current version.
- Open issues already ask for the first step of a split.
  **#30** asks to select which checks run, so a python-only hook need not
  pay for the slow ldd pass (16 comments, still open). **#38**
  asks for python modules outside `usr/lib/python3.X/` (`mod_wsgi`).
  **#39** is the ABI-without-soname case above.

### The split to propose

Each step is useful to rebuild-detector's own users on its own, so none is
a request to carry AURCache's needs:

1. **Rules as data, no behaviour change.** Move each check's description
   into a rules file shipped by the package
   (`/usr/share/rebuild-detector/rules`, one stanza per kind): the path
   template with a version placeholder, the package whose version decides it,
   how the version compares (major.minor or full), the exempt owners
   (`python3[0-9]*`), and the live command that reports the current version.
   `checkrebuild` reads it and behaves exactly as before. This is where
   AURCache's rule table comes from instead of being written here.
2. **Check selection** (#30): `checkrebuild -c python,perl`, and per-kind
   hooks. It falls out of 1 almost for free and is the change upstream
   users asked for.
3. **Machine-readable output**: `package<TAB>kind<TAB>reason` (the reason
   being the missing soname or the stale version directory), so a consumer
   does not scrape the verbose log.
4. **An offline mode**: the same rules, with the facts as input rather than
   asked of the live system: package file lists (the format of a `.files`
   database, which both pacman and `repo.files` already speak) and the
   current version of each deciding package. This is AURCache's case, and
   any build service's (a `repo-add`-maintained repository can check itself
   before its users do).
5. **The ldd pass as a step of its own**: `checkrebuild --ldd <files>`,
   printing `file soname path owner|not-found`, which is the worker step in
   §1, so the worker runs upstream's code rather than ours, including its
   locale and `LD_LIBRARY_PATH` handling.
6. Later, jointly: the symbol check for #39.

The approach: open an issue upstream describing the split and its uses,
referencing #30, and offer to write it. Then send small PRs in this order,
each with tests (upstream has none, and contributing a fixture harness is
part of the offer). Nothing is announced or sent on AURCache's behalf
before the maintainer has said whether they want it.

### How AURCache consumes it

- The **worker** is Arch: it installs `rebuild-detector` in the build
  chroot after `makepkg`, or bind-mounts the script, and runs step 5.
- The **server** image is Debian (`docker/server.Dockerfile:85`), so it
  cannot depend on the Arch package at run time. It takes the rules file
  from a pinned rebuild-detector release at build time
  (`include_str!`), and a CI job reports when a newer release exists, so the
  update is a reviewed bump rather than a silent drift.

### Until then, or if upstream declines

AURCache carries the table above, transcribed from upstream's
four functions, with a tripwire: a CI check pins the `checkrebuild` release
it was last reviewed against and fails when Arch ships a newer one, asking a
person to re-read the diff. That is cheap and catches exactly the drift this
section is about. The design does not wait on upstream: the vendored table
and the upstream rules file have the same shape, so switching is a source
change, not a redesign.

---

## Alternatives considered

- **Run `checkrebuild` itself, in a verification chroot.** Install each
  published package into a current chroot and run `checkrebuild -i aurcache`.
  This needs no parsing of our own, but costs a full install per package per
  check. Installing everything at once is impossible (`-git` and release
  variants conflict). Prebuilt `-bin` packages are flagged forever without a
  baseline. And its ldd check still needs the trigger above to decide *when*
  to run. Its rules and its ldd pass are shared with it instead (see
  above), not its whole-system runtime. It would make a good manual "verify
  this package" action later, and the home of a #39 symbol check.
- **Parse ELF on the server** (`goblin`, `object`). This would give
  DT_NEEDED without a worker change, but the server would then have to
  resolve sonames itself (RUNPATH, `$ORIGIN`, the loader's search order),
  which is what `ldd` does correctly where the libraries live. Rejected per
  the first constraint.
- **Rebuild on any change in `.BUILDINFO`.** This is safe, but `glibc`,
  `gcc-libs` and `bash` move weekly and are in every chroot: every package
  would rebuild every week. That is what the opt-in setting is for, one
  name at a time.
- **Soname `provides` only, no `.files`.** This is cheaper, but it is blind
  to `abseil-cpp`, which is the library `protobuf`'s users actually break
  on.
- **Enable makepkg `autodeps`.** As shown above, it never fires against
  Arch's unprefixed provides.

## Not in this proposal

- **Closing the gap between the official update and our rebuild.** A client
  upgrading `protobuf` before the rebuild lands still gets a broken
  `eternalterminal`. The recorded `LinkedLibrary` rows would let a makepkg
  drop-in add `depends=libprotobuf.so=36.1.0-64`, so pacman refuses the
  `protobuf` upgrade until we catch up, as Arch's own soname dependencies
  do. That blocks `pacman -Syu` outright, so it belongs in its own opt-in
  design.
- **Platforms other than `x86_64`.** `OfficialRepos` fetches `x86_64`
  databases only. Library and interpreter checks for `aarch64`/`armv7h`
  need that platform's databases (Arch Linux ARM). Until then they run for
  `x86_64` artifacts, and `rebuild_on` works anywhere our own repository is
  the source.

## Open questions

1. Should `rebuild_on` compare **pkgver** only (proposed), or also pkgrel?
   Arch bumps pkgrel for rebuilds that can matter (Haskell), and for many
   that do not.
2. When the trigger dedupe stops a loop, should the package's status change
   (a distinct "needs attention"), or is the reason on the build enough?
3. Is the offline mode (step 4 of the split) something upstream wants in
   `checkrebuild`, or better as a separate script in the same package
   sharing the rules file? This is the maintainer's call; the rules file is
   the part that matters to AURCache.
4. `ldd` in the legacy Docker builder: it runs makepkg in a container with
   the dependencies installed, so the same step applies. Is it worth doing
   there, or should the check only run on the chroot worker?

## Implementation order

0. Open the upstream discussion (see [Sharing the rules](#sharing-the-rules-with-rebuild-detector)).
   It runs in parallel with everything below. AURCache starts on the
   vendored table and its tripwire, and switches to the upstream rules file
   and ldd step when they ship.
1. `OfficialRepos` keeps versions and versioned provides (the data model
   change everything else reads).
2. Interpreter rules. These are stateless, with no worker change, and show
   the queueing, reason and pkgrel-suffix path end to end.
3. The `rebuild` counter and the pkgrel `SourcePatch`, with version
   comparison stripping the suffix.
4. The ldd step on the worker (upstream's `--ldd` once it exists, the same
   output format until then), `LinkedLibrary` in `CompleteReport`, the
   `linked_libraries` table and migration, and the provides/`.files`
   decision.
5. `rebuild_on`, reading `.BUILDINFO` from the published artifact.
