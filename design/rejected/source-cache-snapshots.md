# Per-build snapshots of the source cache, and mirrors shared by URL

Two related proposals for the chroot worker's source cache
(`cache/srcdest/<pkgbase>` in the storage pool):

- give each build a snapshot of its package's source cache, and merge what it
  fetched back at the git level when it succeeds;
- on top of that, keep one git mirror per upstream URL, shared between the
  packages that use it.

Status: **Rejected for now** (2026-09-28): not worth its cost. Both were
designed and prototyped as part of `design/proposed/btrfs-snapshots.md` and
split out of it. The design and the measurements are kept here so that
reopening either one does not start from scratch.

---

## Why they were rejected

### Per-build snapshots, merged back with git

What it would have bought:

- **Concurrency: nothing.** The server already refuses to give a worker a second
  build of a package it is building (`srcdest_lock.rs`). `SrcdestLocks` only
  guards against `build-once`, older servers and bugs.
- **Isolating failed builds.** Nothing a failed build fetched would reach the
  cache. That answers a problem that has not been seen. The incidents so far
  (the `google/fonts` stale checkout) came from a mirror being evicted and
  cloned again, which snapshots do not change, and the existing self-heals
  (`wipe_srcdest`, `wipe_borrowed_checkouts`) cover them.
- **Control over git's gc.** This needs no snapshots at all. It is a separate
  question, postponed in `design/rejected/git-mirror-maintenance.md`.

What it would have cost:

- the merge protocol, the ref rules, pins and reflog expiry;
- a lock per cache key;
- a new report field;
- a sizing change for operators. A build's fetches would count against
  `WORKER_BUILD_DISK_MAX`, and a cold fetch is a whole upstream history
  (4.5 GB for `google/fonts`, tens of GB for UnrealEngine).

**Reopen it if** a failed build is seen leaving a mirror that breaks later
builds of its package.

### Mirrors shared by URL

- **It saves disk and nothing else.** A package's first build on a shared
  mirror has no refs of its own to negotiate with, so its fetch downloads the
  whole history anyway (measured: 8 MiB against 212 KiB). Refs parked elsewhere
  in the mirror do not help: makepkg's `fetch -p` prunes them before it
  negotiates. Getting that first download back needs the seeding guard below,
  the most delicate part of the design.
- **The disk saved is small.** `design/implemented/remote-workers.md` measured
  none on the reference server. The motivating case, the armv7 cross-toolchain
  chain, duplicates the gcc and glibc mirrors two or three times: a few GB
  against a 200G default `WORKER_DISK_MAX`. Paying that disk is the simpler fix.
- **It creates a trust boundary that does not exist today.** A build writes its
  copy of the mirror, and makepkg only warns when a fetch fails, then builds
  from whatever refs the mirror holds. Refs merged into a mirror shared by
  several packages would let one package's planted ref be built by another. A
  fast-forward-only rule does not prevent it: a commit on top of the real head
  is a fast-forward. Most of the design below exists to contain a risk that
  sharing itself creates.

**Reopen it if** a worker's duplicated mirrors become a real share of its disk:
several packages cloning the same multi-GB repository, with eviction churning
because of it.

## The design, in brief

Every rule here was prototyped (see below).

### Layout

- A build's `SRCDEST` becomes `<pool>/job-<id>.src`, a snapshot of
  `cache/srcdest/<pkgbase>` in the build's quota group. devtools binds it at
  `/srcdest` as today.
- With URL keys, each git source is a nested snapshot of `cache/git/<key>`,
  placed at `job-<id>.src/<dir>`. `JobDescriptor::vcs_sources` already carries
  the URL and the `dir`.
  - That solves the placement problem `remote-workers.md` parked pooling over:
    makepkg looks only at `$SRCDEST/<name>`, and the directory is keyed by URL
    on disk and by name where makepkg looks. No extra bind and no symlink are
    needed.
  - The key is the URL without makepkg's decorations (`git+`, `#fragment`,
    `?signed`). Nothing else is normalized. The directory name is
    `<last path component>-<first 12 hex of sha256(key)>`.
- Only git mirrors are shared. Plain downloads and hg/svn/bzr/fossil checkouts
  stay per package.
- Kept checkouts' `alternates` name `/srcdest/<dir>/objects`. That path is the
  same in every build, and each build's copy is a fresh snapshot of the merged
  mirror, so a checkout finds every object the merges kept.

### Merging back, after a success only

Under the key's lock:

1. `git fetch --no-write-fetch-head <copy> '+refs/heads/*:refs/aurcache/incoming/<id>/heads/*' '+refs/tags/*:refs/aurcache/incoming/<id>/tags/*'`.
   Only `heads` and `tags` are taken.
2. Move refs by the rules below.
3. Drop the incoming refs.

Notes:

- A local fetch writes fresh objects: no hardlink, no shared extent, and the
  charge lands in the cache (git 2.55 and 2.39).
- Never `git clone --local`: across subvolumes it aborts with `EXDEV` rather
  than copying. The shared mirror is made with `init --bare` and then a fetch.
- Promoting one build's whole copy breaks the other build's kept checkouts
  (`bad object`). That is why the merge is at the git level.
- A failed build merges nothing. A kept checkout whose ref tips the cache lacks
  is wiped.

### Refs: one namespace per package

Objects are shared and refs are not. Each package's refs live under
`refs/aurcache/pkg/<pkgbase>/*` in the mirror, and a lease writes that
namespace into the build's copy as its `heads` and `tags`. Within a namespace:

| Build's value vs the namespace's | Result |
|---|---|
| no such ref yet | take it |
| fast-forward | take it |
| not a fast-forward | take it only if this copy fetched later |
| ref the copy pruned | delete it only if this copy fetched later |

**"Fetched later"** compares the mtime of a **non-empty** `FETCH_HEAD`:

- a failed fetch rewrites `FETCH_HEAD` empty, and such a copy moves no refs;
- a fresh `clone --mirror` writes no `FETCH_HEAD`, and its `packed-refs` mtime
  is used instead;
- the clock is `CLOCK_REALTIME` of the one kernel that holds the pool.

**Seeding.** An empty namespace is seeded from the most recently fetched one. A
successful fetch overwrites and prunes every seeded ref. A build that ran on a
seeded view and whose `FETCH_HEAD` is empty fails before anything is uploaded.

### Pins, reflogs and gc

- `refs/aurcache/kept/<pkgbase>/<arch>/*` hold exactly the ref tips of that
  package's kept checkouts. They are replaced as a set after each build and
  dropped with the tree. With pins, the mirror survives `gc --prune=now`;
  without them, the checkout breaks.
- The worker expires the checkouts' reflogs when it records pins. After a
  prune, `fsck` reports reflog entries that name missing objects, although
  `git fetch` still works.
- `gc.auto=0` in the shared mirror, so the worker runs gc itself, under the
  key's lock.

### Plain files and other VCSs

- Plain files merge one by one: `cp --reflink=never` to a temporary name, then
  a rename. `*.part` files, dotfiles and symlinks are skipped.
- hg, svn, bzr and fossil checkouts merge with `rsync -a --delete`.

### Never measured

- A real makepkg build on an assembled `SRCDEST`.
- Whether `btrfs subvolume snapshot -i` sets the quota group at creation.
- A merge killed halfway.

## Prototype results (2026-09-28)

Kernel 7.2.4, btrfs-progs 7.1, a scratch pool with simple quotas. git 2.55.0,
and 2.39.5 (Debian bookworm, in a container) where marked.

| Check | Result |
|---|---|
| Local `git fetch` from a build's copy into a mirror in another subvolume, as loose objects and as a 30 MB pack (2.55 and 2.39) | new files, link count 1, zero shared extents; the cache's quota group grew by what the build fetched |
| `git clone --local` across subvolumes (2.55 and 2.39) | aborts with `EXDEV`, no copy fallback; within one subvolume it hardlinks |
| `FETCH_HEAD` after a makepkg-style `fetch --all -p` in a bare mirror | written; a no-op fetch rewrites it; a failed fetch rewrites it *empty*; a fresh `clone --mirror` writes none |
| Fetch with no refs as haves, all objects present | the whole history again: 8 MiB against 212 KiB with the package's own refs. Refs under `refs/aurcache/*` are pruned by `fetch -p` before it negotiates (also 8 MiB) |
| Promoting one build's whole copy over the cache | a kept checkout made by the other build: `fatal: bad object refs/heads/makepkg` |
| Git-level merge of two packages' builds into one mirror (force push, deleted branch), in namespaces, with pins, then `gc --prune=now` | namespaces right; the kept checkout resolves every ref and fetches; a force-pushed value nobody borrows is pruned |
| Two builds of one package, the earlier fetcher merged last | the later fetch's value stays |
| The same with the package's pins dropped | the checkout breaks |
| The checkout's reflog after the prune | `fsck`: `invalid reflog entry`; `git fetch` unaffected |
