# Persistent build directories

Plan for keeping a package's build tree between builds, so a long compilation
is not repeated from scratch. Only successful builds keep their tree: an
unsuccessful build discards it, and the retry starts cold (see
`design/implemented/btrfs-snapshots.md`). What the tree saves is the next
version's rebuild, not the retry.

Status: **implemented**, except where noted under "What this does not
solve". The reclaim policy below differs from what was first designed; the
reason is recorded there.

---

## The failure that prompted it

`unreal-engine` compiled for 3h47m, reported `BUILD SUCCESSFUL`, and then died
in `package()`:

```
install: cannot stat '../unreal-engine.sh': No such file or directory
==> Removing chroot copy [/var/lib/aurcache-chroot/job-645-3001993]...done
```

Every byte of those four hours was inside the chroot copy, and the copy was
deleted the moment the build ended. Nothing was recoverable. A retry would have
started again from an empty tree -- and would have hit the same `package()` bug,
because that failure is a PKGBUILD portability defect, not a transient one.

The salvage that *should* have been available is `makepkg --repackage`, which
skips download, extract, `prepare()` and `build()` and runs only `package()`
against the existing `$srcdir`. Minutes rather than hours. It needs the build
tree to still exist.

## What persists today, and what does not

| | keyed by | persists | holds |
|---|---|---|---|
| `SRCDEST` | pkgbase | **yes** | what makepkg *downloads* |
| `BUILDDIR` | -- | **no** | what makepkg *extracts and builds* |

`SRCDEST` is already per-package and persistent
(`<cache>/srcdest/<pkgbase>`, 21 packages and 4.2 GB on the reference
worker). It is bind-mounted into the chroot and survives everything. It is
not shared between packages, deliberately: a single pooled `SRCDEST` was
considered and rejected, because concurrent chain builds race on the same
partial downloads.

`BUILDDIR` is `/build` inside the chroot copy, so it dies with the copy.

For most packages this is barely noticeable -- the sources are in `SRCDEST` and
extraction is local. For `unreal-engine` it is the whole cost, because the
UnrealEngine tree is **not declared in `source=()`**. It is cloned inside
`prepare()`:

```bash
git clone --depth=1 --branch=${pkgver}-release git@github.com:EpicGames/UnrealEngine "${pkgname}"
```

so makepkg's download cache never sees it. `srcdest/unreal-engine` holds only
the 1.59 GB toolchain tarball, while the build needed 129 GB. Everything else is
fetched again on every attempt.

That PKGBUILD is written expecting the tree to survive:

```bash
# Download Unreal Engine source or update if the folder exists
if [[ ! -d "${pkgname}" ]]; then
  git clone --depth=1 ...
else
  ...
  git fetch --depth=1 origin tag ${pkgver}-release
```

The `else` branch has never once executed here.

## Why this is possible at all

makepkg only removes `$srcdir` when `--cleanbuild` is passed:

```bash
if (( CLEANBUILD )); then
    rm -rf "$srcdir"; mkdir -p "$srcdir"
fi
cd_safe "$srcdir"
extract_sources
```

Neither `makechrootpkg` nor AURCache passes it -- our makepkg arguments are
`--noconfirm --noprogressbar --nocolor`, and `makechrootpkg`'s own `-C` is
*checkpkg*, an unrelated flag. So an existing tree is kept and sources are
re-extracted over it. Persistence requires no makepkg change; it requires only
that `/build` outlive the chroot.

## Layout

makepkg derives the tree from `BUILDDIR` (makepkg lines 1295-1300):

```bash
if [[ $BUILDDIR -ef "$startdir" ]]; then
	srcdir="$BUILDDIR/src"
else
	srcdir="$BUILDDIR/$pkgbase/src"
	pkgdirbase="$BUILDDIR/$pkgbase/pkg"
fi
```

Under `makechrootpkg`, `BUILDDIR=/build` and `startdir=/startdir`, so the second
branch applies and the tree is `/build/<pkgbase>/src`.

**makepkg therefore namespaces by pkgbase already.** A per-package host
directory would nest the name twice for no gain. The host side needs to key by
*platform* only:

```
<cache>/builddir/<platform>   →  bound at /build
                                 └── <pkgbase>/src
                                 └── <pkgbase>/pkg
```

Platform, because a build tree holds compiled objects. `SRCDEST` can be
pkgbase-only since downloads are architecture-independent; an x86_64 and an
aarch64 tree sharing a directory would corrupt each other. Keying by platform
also removes the same-package race on a worker that builds a native and an
emulated architecture.

`pkg/` persists too. It is staging output, recreated per run, and harmless.

## On by default

On by default, and the reason it can be is that the risk assumed at first
does not exist.

This is what an AUR helper on a workstation already does: `paru` and `yay` keep
their build trees between builds, across a very large number of users, and
trouble is rare. The chroot is still built from a fresh snapshot every time --
only the tree survives -- so the guarantee that actually matters, a clean
toolchain, is untouched.

Re-applied patches were the worry, and they are **not** a problem. Tested with
`libpng12`, whose `prepare()` does `patch -Np1` against a tarball source: three
consecutive `makepkg -o` runs against one persistent `BUILDDIR` all exited 0
with no "reversed (or previously applied)" anywhere. `extract_sources` unpacks
over the existing tree and bsdtar overwrites, so `prepare()` always sees
pristine sources. A file the tarball contains is restored even if the previous
run scribbled on it.

What survives re-extraction is everything the tarball does *not* contain --
confirmed in the same test: a planted `config.cache` and `png.o` were both
still there afterwards. That is the actual risk, and it is inseparable from the
benefit: leftover object files are what make an incremental rebuild fast, and
also what makes it wrong when compiler flags changed underneath them. A killed
build used to be the other half of that risk, leaving something half-written;
it no longer is, since a failed build discards its tree. What remains produces
a package that is quietly incorrect rather than one that fails loudly, which is
the expensive kind.

That residual risk is real but rare, and it is the same mechanism as the
benefit -- so it is a reason to keep an escape hatch, not a reason to default to
throwing four hours of work away. The setting stays so a package that does turn
out to mind can be excluded without deleting directories on a worker by hand.

Resolved through `ApplicationSettings` like every other package setting, with
the established precedence `Package -> Env -> Global -> Default`. The packages
that want it are the ones where a rebuild costs hours, and there are few.

The setting decides whether the bind happens at all. When it is off the build
gets the ordinary `/build` inside the ephemeral chroot copy and nothing survives
it; when on, the host directory is bound over `/build` and makepkg's own
`<pkgbase>/` namespacing keeps packages apart inside it.

## Reclaiming space

Sizing storage for the packages you build is the operator's job. This is not a
cache that tries to be clever about what to keep; it is a way to collect old and
unused build trees, and **age is the main factor**.

Least recently used first, until the cache is within `WORKER_BUILDDIR_MAX_BYTES`
(default 200 GiB) *and* the filesystem has `WORKER_BUILDDIR_MIN_FREE` bytes
spare (default 50 GiB). Two limits because each is useless alone: a free-space
floor never triggers on a large pool, so the cap is what bounds the cache
whatever the storage underneath it, while a cap cannot see the rest of the
machine, so the floor still covers a small disk or one shared with something
that grew.

**Nothing is evicted while the cache is within both limits.** Disk that nothing
else needs is not worth reclaiming, and a tree kept is a rebuild avoided. It
follows that the tree of a deleted package is left alone until the space is
actually wanted -- which is the right time to notice, since a worker is never
told a package went away and being unused is the only evidence there is. Under
pressure it sorts to the front on its own, because nothing has touched it.

### Ranking by rebuild cost was tried and removed

It seemed obvious that a four-hour tree should outlive a thirty-second one, and
that a huge tree should be given up first once space is short -- which together
point at value density, rebuild seconds per byte. Both premises were wrong.

The numerator was wrong: the stamp recorded *build duration*, but a tree only
saves the part it makes unnecessary. Measured on `libpng12`, a cold build took
19 s and a warm one 11 s, so the tree saved 8 s and not 19 -- and that fraction
varies per package, so it is not a constant to divide out.

With the right numerator the ratio stops discriminating at all. Time saved is
roughly proportional to what a tree holds, because most of it is download the
tree spares you. `libpng12` saves 8 s over 17.9 MB (4.5e-07 s/byte);
`unreal-engine` saves hours over 130 GB (8.3e-08). Five times apart, against the
500x spread build duration had suggested. A ratio that near-constant sorts
nothing, so it is complexity that ranks noise.

An age threshold for "abandoned" trees was removed with it. Age is the primary
key now, so a deleted package's tree reaches the front unaided, and expiring it
on a timer would have broken the rule above by reclaiming space nothing wanted.

Sizes still come from a `.aurcache-size` stamp each build leaves behind, since
the cap needs a total; a tree without one is walked once and stamped. Whole
trees only, never partial contents -- half a tree is worse than none, because
makepkg would treat it as resumable. Never the tree the current build is about
to use. Best-effort: failing to reclaim is reported and the build proceeds,
since refusing to build over it would turn a full disk into an idle worker.

Neither limit bounds a *single* build: `unreal-engine` can grow by ~130 GB after
the check passes. Nothing short of a filesystem quota would.

## A tree's VCS checkouts cannot outlive `SRCDEST`

The two caches are reclaimed independently and against very different budgets
-- sources against 10 GiB, trees against 200 GiB -- but a tree's VCS checkout
is makepkg's `git clone -s` of the `SRCDEST` mirror, so the mirror is its only
object store, reached through `.git/objects/info/alternates`. The mirror is
therefore always what goes first, leaving checkouts whose objects are gone.

Usually nothing notices: the next build re-clones the mirror, and a fresh clone
holds everything the stale checkout still refers to. It breaks when upstream
rewrote a ref. `ttf-google-fonts-git` tracks `google/fonts`, whose `gh-pages`
is a deploy branch force-pushed on every deploy, so a checkout left from six
days earlier still had `refs/remotes/origin/gh-pages` at a commit that had been
pushed over -- and a fresh clone has no such commit. makepkg's `git fetch` in
that checkout fails its connectivity check before the build starts:

```
fatal: bad object refs/remotes/origin/gh-pages
error: /srcdest/fonts did not send all necessary objects
==> ERROR: Failure while updating working copy of fonts git repo
```

and fails identically on every retry, because nothing was clearing the
checkout. It cost two builds before it was understood, and it would have gone
on failing.

So `Cache::wipe_srcdest` now takes the borrowed checkouts with it: under every
platform's `builddir/<platform>/<pkgbase>/src`, each entry whose `alternates`
names the in-chroot `/srcdest` mount. Only those. A checkout is a local clone
of a mirror and costs seconds to make again, while the compiled output beside
it is the hours this whole feature exists to save, and it borrows nothing --
`unreal-engine`'s tree, cloned in `prepare()` rather than declared in
`source=()`, has no `alternates` file at all and is untouched. The tree's size
stamp is dropped rather than corrected, so the next reclaim measures instead of
over-counting a tree that just shrank.

That used to be only half the story: a checkout left stale by an *earlier*
re-creation could outlive a healthy mirror forever, failing every retry from
inside makepkg where only the worker could reach it. The worker watched the
build's streams for the failure's own signature and wiped just the borrowed
checkouts after such a failure. It no longer does: an unsuccessful build now
discards the whole tree, and the next build starts cold. A half-updated tree
can fail every retry identically -- a stale checkout, or an output a killed
compiler left truncated but newer than its inputs -- and telling the two apart
costs more than a cold rebuild saves. See
`design/implemented/btrfs-snapshots.md`. The mirror wipe above stays: wiping a
mirror still orphans the checkouts a kept tree borrowed from it.

## What this does not solve

A retry still re-runs `prepare()` and `build()`. `prepare()` does
`git reset --hard`, which resets tracked files but leaves untracked build output
(`Intermediate/`, `Binaries/`), so UnrealBuildTool should skip most compilation
-- but that is an expectation, not a measurement, and it should be measured
before being claimed. The guaranteed saving is the clone; the incremental-build
saving is likely and unproven.

Exposing `--repackage` as a per-build action would make the salvage explicit
rather than depending on the build system's incrementality. That is a separate
change and is not proposed here.

## Rejected: making `BUILDDIR` equal `startdir`

makepkg takes a different branch when `BUILDDIR` *is* `startdir`, giving
`srcdir=$BUILDDIR/src` -- the layout a plain `makepkg` produces, where `..`
from `$srcdir` is the directory holding the PKGBUILD and its local sources.
Setting `BUILDDIR=/startdir` in the drop-in would therefore make PKGBUILDs like
`unreal-engine`'s `../unreal-engine.sh` resolve, and it would work: `/startdir`
is bind-mounted read-write (`--bind`, not `--bind-ro` -- the "not writeable"
warning is file ownership), makepkg already runs with `cd /startdir`, and
`makepkg.conf.d` drop-ins are sourced after `/etc/makepkg.conf`, so they
override the `BUILDDIR=/build` that `makechrootpkg` appends.

It is rejected because it makes AURCache lie. A package that built here would
still fail under `extra-x86_64-build`, or for anyone running `makepkg` in a
clean chroot -- so the service would ship packages whose PKGBUILDs are broken
and hide the evidence, moving the failure onto users' machines. `$startdir` is
discouraged in PKGBUILDs for exactly this reason; makepkg copies local sources
into `$srcdir`, and `$srcdir/unreal-engine.sh` is the portable form.

The right fix for such a package is to patch the PKGBUILD, which also benefits
everyone else building it.

Symlinking `/startdir`'s contents into `/build/<pkgbase>/` was considered and is
strictly worse: it produces a hybrid layout matching neither convention, and
risks shadowing the `src` and `pkg` directories makepkg creates there.

## Rejected: preserving the chroot copy

The first instinct after the `unreal-engine` failure was to stop passing
`makechrootpkg -T` on failure, keeping the chroot for post-mortem.

It is the wrong lever. What had value was `$srcdir`; the chroot was merely the
container it sat in. The chroot's own contents -- `base-devel`, `git`,
`openssh` -- are reproducible and cheap, a btrfs snapshot of the base costing
close to nothing. With a persistent `BUILDDIR` the chroot becomes genuinely
disposable, `-T` stays correct, and the thing worth keeping is no longer inside
the thing being deleted.

Keeping a failed chroot still has some value -- the exact set and versions of
installed `makedepends` explain some failures -- but it is a diagnostic
convenience, not recovery, and it would not have saved the four hours in any
automatic way.
