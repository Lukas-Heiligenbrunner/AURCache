# Git's own maintenance in the source cache's mirrors

makepkg keeps a bare mirror of each git source in the package's source cache
(`cache/srcdest/<pkgbase>/<dir>`). For a package with a persistent build tree,
it also keeps a checkout made with `git clone -s`. That checkout owns no objects
and borrows them from the mirror through `alternates`. Git's automatic
maintenance runs in the mirror without knowing the checkout exists.

Status: **Postponed** (2026-09-28). The mechanism is real but has not been
observed, and when it happens an existing self-heal recovers after one failed
build. Pick this up when a failure is traced to it (see "When to pick it up").
Split out of `design/proposed/btrfs-snapshots.md`.

---

## The potential issue

Every `git fetch` ends with `gc --auto`. Once a mirror passes git's thresholds
(6700 loose objects or 50 packs), that fetch repacks the mirror, and **prunes**
unreachable objects older than two weeks. The mirror's gc decides what is
reachable from the mirror's own refs only. A kept checkout's refs
(`refs/remotes/origin/*`, its `makepkg` branch) are invisible to it.

**Within one build**, with nothing shared between packages:

1. makepkg's host-side download runs `git fetch --all -p` in the mirror.
   Upstream has force-pushed, so the mirror's branch moves, and the old commit
   becomes unreachable in the mirror.
2. That same fetch ends with `gc --auto`. If the thresholds are passed this
   time, it prunes the old commit, provided it was fetched more than two weeks
   ago.
3. Inside the chroot, makepkg runs `git fetch` in the kept checkout. Its
   `origin/<branch>` still points at the pruned commit, so the fetch fails with
   `bad object … did not send all necessary objects`. That is the same
   signature as the `google/fonts` incident, although that one was caused by
   the mirror being evicted and cloned again, not by a prune.

**A longer window: one package kept on two architectures.** Both kept trees
borrow the same mirror (`SRCDEST` is per pkgbase, not per platform). Building
x86_64 moves the mirror's refs. The aarch64 checkout keeps pointing at the old
values until aarch64 builds again, possibly weeks later, and any `gc --auto`
in between can prune what it borrows.

A lesser cost of the same mechanism: the repack runs in the middle of
whichever build's fetch triggered it, and that build pays its time. On a
4.5 GB mirror this could be minutes. It has not been measured.

## Why it is postponed

- **Not observed.** It needs a force-push, a kept tree, and auto-gc happening
  to trigger on the right fetch.
- **Already recovered from, at a price.** Any unsuccessful build of a package
  with a persistent tree discards that tree, so the retry starts cold and
  makes a fresh checkout from the mirror, which the retry's own fetch fills
  again (`design/implemented/btrfs-snapshots.md`). The cost is one failed
  build and a cold rebuild: seconds for most packages, hours for one like
  unreal-engine. (Before that, the worker watched the build's output for the
  `bad object` signature and wiped only the borrowed checkouts, which cost
  seconds; that watch went when discarding took its place.)

## When to pick it up

- a stale-checkout failure traced to a prune rather than to an eviction,
  especially on a package whose cold rebuild is long, or
- a build log showing git repacking a large mirror mid-build, costing real time.

## Candidate fixes

Measured on git 2.55 (2026-09-28):

- with `gc.auto=0` and `maintenance.auto=false`, a fetch starts no maintenance;
- `git gc --prune=never` keeps a force-pushed-away commit, in a cruft pack.

**Minimal:** `gc.pruneExpire=never` on each mirror, set by the worker at lease
time (before `makechrootpkg` runs) for the job's existing mirrors
(`JobDescriptor::vcs_sources` names their directories). Auto-gc still runs, so
builds still pay for the occasional repack, but it never deletes anything.
A mirror's history then goes only when the cache budget evicts the whole
mirror, and `wipe_srcdest` already wipes the borrowed checkouts along with
it. The cost is disk: force-pushed and deleted history stays in a cruft pack,
and it counts toward the mirror's size, so eviction sees it.

**Fuller:** git never starts maintenance at all (`gc.auto=0`,
`maintenance.auto=false`), and the worker runs it itself:

- from the detached cache task that runs `evict` at each job start;
- only while holding the package's `SrcdestLocks` guard, taken with
  `try_lock`, so a mirror is skipped while its package builds;
- `git repack -d --geometric=2` past 50 packs, and `git gc --prune=never`
  weekly.

This also takes repacks out of builds. It is worth it only if the second
trigger above is seen.

Open for when this is picked up: how long `gc --prune=never` takes on the
largest mirror a worker holds (`google/fonts`, 4.5 GB), and how large its
cruft pack gets.
