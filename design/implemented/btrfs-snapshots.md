# More snapshots in the worker's pool

Every chroot worker now keeps everything it stores in a btrfs storage pool
(`design/implemented/build-disk-quota.md`), so btrfs snapshots are available on
every worker, not only on hosts that happen to be btrfs. A snapshot is instant,
and under simple quotas a new snapshot is charged only for what is written into
it afterwards. That makes it cheap to get three things: a rollback point,
isolation between writers, and a copy to inspect later. This doc collects the
places where the worker could use them, and settles how each one behaves.

Status: **Implemented** · Last updated: 2026-09-28

Split off from this doc:
- `design/rejected/source-cache-snapshots.md`: per-build snapshots of the source
  cache merged back with git, and mirrors shared by URL. Designed and
  prototyped, then rejected for now as not worth their cost.
- `design/rejected/git-mirror-maintenance.md`, postponed: git's auto-gc in a
  source mirror can prune objects a kept checkout still borrows. It is a
  separate issue that snapshots do not touch.

Decided (details in each section; measurements in "Prototype results"):

- **Base refresh** snapshots `root` into `root.next`, upgrades and checks it,
  then swaps it in with `renameat2(RENAME_EXCHANGE)`, which works on subvolume
  roots, as the worker's own user. No fallback is needed. `root.lock` and every
  user of it go. A refresh in flight never holds up a build start.
- **Kept build trees** are moved beside the keep on a kept failure, and
  discarded on any other unsuccessful build. Either way the next build starts
  cold. Moving needs no snapshot, so the `.pre` snapshot comes only with
  per-package rollback, which is deferred. Its rules are written down below so
  they are ready.
- **Failed builds** are kept by renaming `job-<id>*` to `kept-<id>*`, with the
  build tree moved to `kept-<id>.build` after the chroot. The keep runs from
  the chroot directory's mtime, with no sidecar file. Canceled, timed out and
  out-of-disk builds are not kept, and kept builds are the first thing deleted
  when the pool needs room.
- One naming scheme and one sweep cover every new name. See "Crash recovery".

---

## Where snapshots are used today

One place: each build's chroot is a snapshot of the base (`<pool>/job-<id>` from
`<pool>/root`), made by the worker before `makechrootpkg` runs and deleted when
the build ends. Everything else in the pool is written in place:

- The base chroot is upgraded in place by `arch-nspawn root pacman -Syu`, under
  devtools' `root.lock`.
- Each package's source cache (`cache/srcdest/<pkgbase>`) and kept build tree
  (`cache/builddir/<arch>/<pkgbase>`) is a subvolume, written directly by the
  build that uses it. Builds of the same package are serialized on the source
  cache (`SrcdestLocks`).

## Accounting, which every idea below has to respect

Simple quotas charge an extent to the subvolume that wrote it, for as long as
the extent exists, and never move that charge:

- **A snapshot charges nothing on its own.** What it shares with its origin
  stays charged to whichever subvolume wrote it.
- **A deleted subvolume's group lingers while its extents do.** When something
  else still references its extents (a snapshot, a reflink), its level-0 group
  stays as a `<squota space holder>` carrying that charge, and it keeps its
  relations to the groups above it. So the charge stays under the pool's total,
  as long as we never destroy a group that still carries charge. The pool code
  only destroys groups btrfs agrees are empty (`tidy_groups`, and
  `qgroup clear-stale`, which was checked to leave a charged space holder
  alone), which keeps this true.
- **"Something else" includes the next generation of the same thing.** A
  subvolume snapshotted from an older one shares every extent neither has
  rewritten, and those stay charged to the older one after it is deleted: its
  space holder lives as long as the newer one keeps those files, not just as
  long as the builds that were running. Measured on a refreshed base: 257 MB of
  a 308 MB base stayed on the retired base's space holder after every build had
  ended, and the current base's own group showed only the 51 MB its refresh
  wrote. The total is right throughout. What is wrong is any figure read from
  one subvolume's group as "the size of this thing". Section 1 and the deferred
  rollback in section 2 both run into this.
- **Never reflink out of a build into long-lived storage.** A reflinked extent
  stays charged to the build that wrote it, so it is invisible to the
  destination's figures. This is why a job's pacman cache already lives inside
  the `pacman-pkg` subvolume rather than the build's. The same rule applies
  below: data moving from a build into a cache is copied, never reflinked, and
  never promoted by snapshotting the build's subvolume.

## 1. Refresh the base as a new snapshot and swap it in

**Today.** A refresh upgrades `root` in place. It takes `root.lock` exclusively;
a build start that finds it held shared (for the instant of its own snapshot)
defers the refresh. An upgrade that breaks halfway (a failing hook, a
half-installed package, a full disk) leaves the base broken for every build
after it, and nothing puts it back. A non-zero `pacman -Syu` is only warned
about (`ensure_base_chroot`), so the broken base is used as it is.

### The refresh

Under the refresh exclusion (below):

1. **Recover** whatever a previous refresh left (see "Crash states").
2. `btrfs subvolume snapshot root root.next`, then `charge_to_total(root.next)`
   at once, before anything is written into it. The snapshot is made at the
   pool's top, which the total does not cover on its own.
3. **Upgrade:** `arch-nspawn root.next pacman -Syu --noconfirm`. It must exit 0.
   devtools does not care what the directory is called.
4. **Multilib:** `ensure_multilib(root.next)`, best-effort as today. Its failure
   off x86_64 is expected and is not a failed check.
5. **Check**, inside `root.next` with `arch-nspawn`, so the checks also run the
   binaries that were just upgraded:
   - `pacman -Dk`: every installed package's dependencies are satisfied and
     nothing conflicts;
   - `pacman -Q base-devel`: the build toolchain is still installed;
   - `sudo -u nobody true`: the path `makechrootpkg`'s `chrootbuild` takes to
     drop to the build user works (a broken sudo, PAM or glibc fails every
     build);
   - no `/var/lib/pacman/db.lck` is left.
6. **Swap:** `renameat2(root.next, root, RENAME_EXCHANGE)`, in-process
   (`libc::renameat2`). The pool's top directory belongs to the worker, and an
   exchange within one directory needs no rights over the subvolumes
   themselves, so no sudo is needed (measured). The path `root` is never
   absent: it names the old base, then the new one. Afterwards `root.next`
   names the old one.
7. **Retire:** delete `root.prev` if there is one, then rename `root.next` to
   `root.prev`. The deletion comes first because a rename onto a non-empty
   directory fails with `ENOTEMPTY` (measured).
8. Stamp `last_refresh`.

If any of steps 2 to 5 fails, `root.next` is deleted and `root` stays exactly as
it was. The failure is reported as a refresh warning is today (`report_warning`,
to the build that triggered it), and `last_refresh` is still stamped: a refresh
that fails will fail again, so the next attempt waits for the interval rather
than running before every build. Builds are not exposed to the stale base by
this. Each one still runs its own `makechrootpkg -u` in its snapshot, where the
same failure shows up in that build's log.

If the exchange itself is refused, that is the same as a failed check:
`root.next` goes and `root` stays. It exists on every kernel that has simple
quotas (btrfs gained `RENAME_EXCHANGE` in 4.7; squota needs 6.7), so no
two-rename fallback, and no `ENOENT` retry in `lease()`, is specified. Two
renames would open a window with no `root` at all, which is the thing the
exchange is for.

A build snapshots whatever `root` is at the instant it asks, before or after
the swap, and is never exposed to a base halfway through an upgrade. A build
already running on the old base is unaffected by the exchange, and so is any
file it holds open or any directory it is in (measured).

`root.prev` is a rollback point: renaming it back over `root` (by the same
exchange) restores the last base. That can become an explicit operation
(`aurcache-cli worker …`) later. Nothing does it automatically.

### Exclusion without `root.lock`

A snapshot is atomic and the swap is atomic, so nothing about *taking* a
snapshot needs to be excluded from a refresh any more. What still needs
excluding is two refreshes at once:

- **Within the worker:** the existing `last_refresh: Mutex<Option<Instant>>`
  in `Chroots` stays the in-process exclusion, and is where "is it due" and
  "refresh it" remain one decision. A job start takes it with `try_lock`. If
  it is held, a refresh is already running, and the job leases the current
  `root` at once instead of queueing behind a 13-second upgrade. The one
  exception is a pool with no base yet: that start waits (`lock().await`),
  because there is nothing to snapshot until `mkarchroot` is done.
- **Across processes:** `build-once` can share a pool with a running worker
  (`ONE_SHOT_BUILD_ID`). The refresh also takes a `flock` on
  `<pool>/refresh.lock`, a worker-owned file, without waiting. If it is busy,
  the refresh is skipped as above. The kernel drops the lock when its holder
  dies, so a lock left by a crash never exists. That is also what lets step 1
  treat any `root.next` it finds as dead.

What goes, all in `aurcache-worker` (the docker worker has no chroot and none
of these):

| Item | Where | Why it existed |
|---|---|---|
| `BaseLock`, `try_lock_base` | `chroot.rs` | the refresh's exclusive `root.lock` |
| `share_base_chroot`, `open_base_lock` | `chroot.rs` | a lease's shared `root.lock` for its snapshot |
| `lock_beside` and its test | `chroot.rs` | the path of `root.lock` |
| the `BaseLock` match in `Chroots::refresh`, the shared lock in `Chroots::acquire` | `chroots.rs` | the two callers |
| `BASE_CHROOT_LOCK` | `chroot.rs` | serializing creation and refresh within a process: now the `last_refresh` mutex |
| the "caller must hold `root.lock`" contract | `ensure_base_chroot`'s doc | |

devtools itself only takes `root.lock` in `mkarchroot` (first creation, which
runs under the refresh exclusion) and in `makechrootpkg`'s `sync_chroot`, which
never runs here: it only runs when the copy is missing or with `-c`, and the
worker always makes the copy itself and never passes `-c` (checked in devtools
1.5.1). `arch-nspawn` takes no lock. The `root.lock` file `mkarchroot` left is
deleted by the startup sweep.

### Crash states

The recovery in step 1 runs when a refresh starts and in the startup sweep,
both while holding `refresh.lock`. It reads the two subvolumes' UUIDs
(`btrfs subvolume show`):

| Found | What happened | Recovery |
|---|---|---|
| `root.next`, whose Parent UUID is `root`'s UUID | the refresh died before the exchange: an unfinished, unchecked base | delete `root.next` |
| `root.next`, and `root`'s Parent UUID is `root.next`'s UUID | the refresh died between the exchange and the retire step: `root.next` is the previous base | delete any `root.prev`, rename `root.next` to `root.prev` |
| `root.next`, neither relation holds, or a UUID cannot be read | cannot tell | delete `root.next` (the cost is losing the rollback point) |
| `root.prev` | the rollback point | keep |
| no `root` | first start, or an operator removed it | `mkarchroot`, as today; any `root.next` is deleted first |

After an exchange, `root`'s Parent UUID is `root.next`'s UUID (measured), so the
two middle cases cannot be confused. A recovery that fails anyway (a delete or
a rename refused) is a failed refresh, never a failed build: with a base, it is
warned about and the base is used as it is until the interval comes round;
without one, `mkarchroot` makes it regardless, since what could not be cleared
is only ever `root.next`. The invariant the whole sequence keeps is
that `root` is always a complete, checked base.

### Accounting

- `root.next` goes under `2/0` as soon as it is made (step 2), as `mkarchroot`'s
  base does today (`charge_to_total`). What the upgrade writes is charged to it.
  After the exchange nothing needs reassigning: groups follow subvolume ids, not
  names.
- `root.prev` keeps its group under `2/0`. When it is deleted, its group becomes
  a space holder for every extent something still references: builds'
  snapshots of it, and, for as long as they last, the files the newer bases
  never rewrote (see "Accounting" above). A space holder stays under `2/0`, so
  the total is right.
- Such groups accumulate: one level-0 space holder per past base that wrote an
  extent still in use, emptied when the last file it wrote is replaced. In
  practice that is bounded by how many refreshes a package's lifetime spans
  (hundreds a year at most), and btrfs handles that many groups. `clear-stale`
  removes each one once it is empty. `tidy_groups` needs no change: it only
  considers level-1 build groups, and base groups are level 0.
- The consequence is that the base's size can no longer be read from `root`'s
  group: it holds only what the last refresh wrote. Nothing reads it today (a
  build's report splits its own usage, never the base's). If it is ever shown,
  it has to be derived (the total minus the caches' and builds' groups), not
  read from one group.
- `root.prev` is the second thing reclaimed when a build needs room, after kept
  failed builds and before any cache (section 3).

**Cost.** One snapshot per refresh (milliseconds), plus the retired base's
exclusive extents until the next refresh replaces `root.prev`: roughly one
refresh's upgraded packages and sync databases.

**Rejected before, and why that no longer holds.** `overlay-chroot.md`
rejected "a new base per refresh" because on the filesystems it targeted a
copy was a full write of the chroot (~62 GB a day at a 30-minute interval).
Inside the pool a copy is a snapshot, so the objection is gone.

## 2. Discard a kept build tree when its build fails

**Today.** A build writes its kept tree in place. One that dies partway leaves
a half-updated tree, which makepkg treats as resumable next time, and it can
fail every retry identically:
- a stale checkout, handled after the fact by `wipe_borrowed_checkouts` and the
  stale-checkout detection in `job.rs`;
- an output a killed compiler left truncated but newer than its inputs, which
  make and ninja then treat as built. GNU make deletes a half-built target when
  it is interrupted by `SIGTERM` or `SIGINT`, but not on `SIGKILL`, and
  `SIGKILL` (`cgroup.kill`) is how the worker ends a build.

**Decided: move the tree into the keep, or discard it.** The tree's fate is
decided in the same place as the lease's, after the keep decision: kept means
the tree moves to `<pool>/kept-<id>.build`, anything else means the tree is
deleted (`subvolume delete`, instant). Either way the package finds no tree
and the next build starts cold, which is always safe: failure, cancel,
timeout, out-of-memory and out-of-disk all go through a kill or a
half-finished step, so every one of them can leave the tree half-updated. The
keep then carries its tree and goes away with it; without the move, an
operator entering `kept-<id>` would find `/build/<pkgbase>` empty, and for
those packages the tree is most of what is worth inspecting.

The move is one rename, done by the pool inside the keep (`Pool::keep_build`),
after the chroot is renamed: nothing ever sees a `kept-<id>.build` without its
`kept-<id>`, which the expiry at every other build's lease would take for an
expired keep. It is instant for a subvolume, which keeps its id (and so its
quota group) across the rename, so the space stays counted and kept builds
stay the first thing reclaimed. It runs as root (`mv --no-copy -T`): a build
that became root in its chroot can own its tree's top, and `--no-copy` keeps
it a rename or nothing. A plain-directory tree -- from before trees were
subvolumes, living inside the cache subvolume -- cannot cross into the pool's
top (`EXDEV`), stays where it is, and is discarded with everything else the
keep did not take; those disappear as they get rebuilt anyway. A tree path
outside the pool is refused, as every path the pool deletes is its own.

**This needs no snapshot.** Moving and discarding are a rename and a delete.
The `<tree>.pre` snapshot the first draft of this section proposed only
matters for a *rollback*, which is deferred.

**The cost, stated plainly:** today a failed build of a long package leaves its
compiled output for the next attempt. After this, every failure of a
`persistent_builddir` package costs a cold build next time: hours for
unreal-engine. That is the case the deferred rollback, or a per-package "keep",
would be for. A kept failure at least keeps the output inspectable until the
keep expires.

### Deferred: per-package rollback

If it is ever needed, it becomes a package setting,
`BUILDDIR_ON_FAILURE = discard | rollback`. It is resolved on the server
through `ApplicationSettings` (Package → Env → Global → Default) and sent in
`JobDescriptor` next to `persistent_builddir`, which is resolved and sent the
same way. What it would take, settled now so it does not have to be
re-derived:

- **Name:** `cache/builddir/<arch>/.pre.<pkgbase>`, snapshotted from the tree
  at lease time. makepkg refuses a `pkgname` that begins with a dot, so no
  tree can have that name (`lint_pkgbuild/pkgname.sh`). The scans skip only
  the set-aside suffix today (`is_set_aside`), so they, and
  `reclaim_builddirs`, must learn to skip dot-prefixed names too.
- **Orphans:** a `.pre.<pkgbase>` with no running build of that package means
  the build died with the worker. The startup sweep and every lease on that
  arch discard both the tree and the `.pre`. The sweep never rolls back: a
  crash says nothing about whether the tree was worth keeping.
- **Rollback** is a delete of the tree and a rename of `.pre` into its place,
  followed by a size stamp: `record_builddir_size` walks the tree, and eviction
  prefers the stamp for it. This is needed because the restored tree's own
  group reads nearly empty, its contents being charged to the deleted original
  (see "Accounting").
- **Reserve up to twice the tree.** While the build runs, every extent it
  overwrites stays charged to the tree as long as `.pre` holds it. So before the
  build, `reclaim_builddirs` has to make room for this tree's size a second
  time, and the pool-wide check ("evict until the build's limit fits under the
  total") has to add it. The caches' budgets are eviction targets, not qgroup
  limits, so it is the total that must have the room.

## 3. Keep a failed build's chroot for a while

**Proposal.** When a build fails, keep its subvolumes for `WORKER_KEEP_FAILED`
(a duration; unset by default, which keeps nothing) instead of deleting them at
once. An operator can then enter the exact state it failed in:
`systemd-nspawn -D <pool>/kept-<id> --bind=<pool>/kept-<id>.build:/build/<pkgbase>`,
with the workdir at `<pool>/kept-<id>.data` (and no `--bind` when the build
kept no tree).

- **Kept means renamed.** At the end of a failed build the worker:
  1. `touch`es `job-<id>`, before any rename, so a `kept-<id>` is never
     visible with the build's old mtime;
  2. renames `job-<id>` → `kept-<id>`, and likewise `.data`;
  3. moves the persistent tree to `kept-<id>.build`, when the build has one
     (section 2);
  4. `chmod 0700` on each, and on `kept-<id>.build` when there is one (see
     "Credentials");
  5. removes `job-<id>.lock`.

  Anything the keep did not take -- no keep, a keep that failed, a tree that
  could not move -- is discarded after it, and the build's log says why a keep
  failed.

  A rename keeps the subvolume ids, so the group `1/<1000+id>` keeps its members
  and its limit (measured). A kept build can never grow, and it stays under the
  total. The name is what tells a kept build from a crash leftover, which the
  startup sweep must be able to do without any record.
- **Keep-until is `mtime(kept-<id>) + WORKER_KEEP_FAILED`, with no sidecar.**
  The `touch` sets the mtime to when the build ended, and a rename leaves a
  directory's own mtime alone. The mtime of the chroot's
  top directory only moves when an entry directly under `/` is made or removed,
  which neither makepkg nor devtools does after the build. The setting is read
  at sweep time, so lowering it shortens keeps already made. `touch` extends
  one, and so does an operator's own work at the top of the chroot. That is
  harmless, and arguably what they want.
- **Not kept:**
  - canceled builds;
  - timed-out builds;
  - out-of-disk builds (`disk_reason` is `Some`): the largest chroots, with the
    least to show, since the cause is already known;
  - one-shot builds (their id is fixed, so the next run would collide).

  Out-of-memory failures are kept: small on disk, and the state they died in is
  the interesting part.
- **The sweep** keeps a `kept-<id>` still within its time and deletes one past
  it, or every one when the setting is unset. It runs at startup, and at every
  lease (where `tidy_groups` already runs), so no new timer is needed.
- **Room first comes from kept builds.** Before a build, the worker evicts
  until the build's limit fits under the total. It now deletes, in order: kept
  builds, oldest first; then `root.prev`; then caches, as today.
- **`tidy_groups`** must treat a `kept-*` entry as present. Otherwise it tries
  to destroy the kept build's group at every lease. btrfs refuses that with
  `EBUSY` while the group has live members (measured), so nothing would be
  lost, but it would be retried and logged at every lease. The naming change
  below fixes this.
- **Where it is shown.** `CompleteReport` gains
  `kept: Option<KeptBuild { path, until, tree }>` (`aurcache-common`, stored on
  the build), where `tree` is the moved tree's path when the build kept one.
  The build's page says "kept on `<worker>` at `<path>` until `<time>`, or
  sooner if the worker needs the room", and shows the one command that enters
  the failed state, with the `--bind` when there is a tree. A "debug on
  worker" action stays deferred.

### Credentials

Nothing secret is left in a kept chroot that the build did not already have:

- The SSH key lives in `<data_dir>/secrets`, outside the pool, and is never bound
  into a chroot (`credentials.rs`). It is used through the agent.
- The agent's socket *directory* is bound in (`agent.rs`). A bind mount ends
  with the container, so the kept chroot holds an empty mount point.
- The GnuPG replica is public keys, in the cache subvolume, and is wiped with
  the job (`wipe_gnupg_job`) whether or not the build is kept.
- The makepkg drop-in (the worker's makepkg settings) is inside the chroot.
  So is whatever the PKGBUILD fetched *with* the credential: a private
  repository's source, in `.data` -- and, for a persistent build, the moved
  tree in `.build`, which is why it gets the same `0700`.

The last point is why kept builds are `0700`. A running build's chroot is
readable by any local user of the worker host for as long as the build runs.
A kept one would be readable for days.

**Cost.** Up to one build's disk per kept failure, for the time set, and the
first thing reclaimed.

## Crash recovery: every name in the pool

`build_of()` becomes a parser for every name the pool uses:
`enum Entry { Build { id, part }, Kept { id, part }, Base(Root | Next | Prev), RefreshLock, DevtoolsLock }`,
with `part` one of chroot, `.data`, `.lock` and `.build`. `sweep`, `tidy_groups`
(which counts a `Build` or a `Kept` entry as present) and `ensure_subvolume`
(which refuses any name that parses) all use it. `ensure_subvolume` already
refuses dots, so it only has to learn `kept-`.

| Name | Made by | A leftover means | Handled by | When |
|---|---|---|---|---|
| `job-<id>`, `.data`, `.lock` | lease | a build that died with the worker | delete (recursive), group destroyed once empty | startup (all not running); lease (its own id) |
| `kept-<id>`, `.data`, `.build` | a failed build | a kept failure | delete when past `WORKER_KEEP_FAILED`, or all if unset | startup; every lease |
| `root.next` | refresh | an unfinished base, or the previous base after the exchange | the UUID rule in section 1 | startup; every refresh (under `refresh.lock`) |
| `root.prev` | refresh | the rollback point | kept; replaced by the next refresh; reclaimed for room | |
| `refresh.lock` | refresh | nothing: a `flock` dies with its holder | left | |
| `root.lock` | `mkarchroot` | unused since the swap | delete | startup |
| `cache/builddir/<arch>/.pre.<pkgbase>` | rollback (deferred) | a build that died | discard the tree and it | startup; lease on that arch |

## Test plan

Unit tests run with `cargo test`. "Root-gated" means `AURCACHE_POOL_TESTS` in
`aurcache-chroot` (or the worker's pool tests), run by `scripts/test-kernel.sh`.

1. **Base refresh**
   - Root-gated:
     - the exchange swaps the base atomically: lease in a loop while
       refreshing, and assert every snapshot holds all old or all new marker
       files;
     - a failing check leaves `root` byte-for-byte and deletes `root.next`;
     - both crash states built by hand: an unfinished `root.next` is deleted,
       a post-exchange one becomes `root.prev`;
     - the total is unchanged across a swap, and the retired base's space
       holder stays under `2/0` and survives `clear-stale`.
   - Unit:
     - the `Entry` parser;
     - a leftover `root.next` classified by UUIDs, an unreadable one as
       unknown, so it is deleted rather than failing every refresh;
     - a refresh in flight makes a job start lease at once (the `try_lock`
       path), while a missing base makes it wait.
   - `test-e2e-hybrid.sh`, since the chroot builder changes.
2. **Kept-tree move or discard**
   - Unit: a moved tree lands beside the keep with its stamp dropped, a missing
     tree moves nothing, and an unmovable tree is discarded instead.
3. **Kept failures**
   - Root-gated:
     - a kept build keeps its group and limit;
     - a keep carries its moved tree: locked down with the rest, and removed
       with it on expiry;
     - the sweep keeps one within its time and deletes an expired one, at
       startup too;
     - `tidy_groups` leaves kept groups alone;
     - room-making deletes kept builds before `root.prev` and before caches;
     - the tops are `0700`; a plain-directory tree stays out of the keep and is
       never copied; a tree outside the pool is refused.
   - Unit: which outcomes are kept; nothing secret (the build credential, the
     generated key, the agent's socket directory) lives under the pool, so no
     keep can hold it. The agent's socket reaches a build only as a bind
     mount, which ends with the container; no unit test can see that, and no
     e2e scenario covers it yet.

## Prototype results (2026-09-28)

A root prototype on the local worker (kernel 7.2.4, btrfs-progs 7.1): a sparse
4G image, `-m single`, simple quotas, a `2/0` total. A 300 MB base with 3000
files stood in for the chroot. The git prototypes run in the same session are
recorded with the designs they belong to (see the top of this doc).

| Check | Result |
|---|---|
| `renameat2(RENAME_EXCHANGE)` on two subvolume roots | works, and as a non-root user owning the pool's top directory |
| During the exchange: a build's snapshot of the old base, a file held open in it, a process whose cwd is inside the old base | all unaffected; a snapshot of `root` taken after it is of the new base |
| Exchange with `root.next` missing | `ENOENT`, nothing changed |
| Rename of a subvolume onto an existing `root.prev` | `ENOTEMPTY`: the old one is deleted first |
| UUIDs after the exchange | `root`'s Parent UUID is `root.next`'s UUID, which tells "after the exchange" from "unfinished" |
| Deleting the old base while a build's snapshot shares it | its group becomes a space holder under `2/0`; the total is unchanged |
| After every build ended | the space holder stays with 257 of 308 MB: the new base shares the old base's unchanged files, which stay charged to the old base. `root`'s own group shows 51 MB, what the refresh wrote |
| `qgroup clear-stale` with that space holder | leaves it (only empty groups go) |
| Rename `job-7` → `kept-7` | group membership and limit kept |
| `qgroup destroy` of a group with live members | `EBUSY` |

What this changed in the design:
- The swap needs no fallback, and the refresh needs no sudo for it.
- Retiring `root.prev` deletes before it renames.
- Crash recovery reads UUIDs rather than guessing.
- The base's size can no longer be read from its own group.
- Discard-on-failure needs no snapshot.

Still to measure before implementing:
- How long the health checks take on a real base.

## Rejected: a per-package chroot with its dependencies installed

Keeping, per package, a snapshot of the chroot after its dependencies were
installed, and snapshotting from that next time, would skip the dependency
install step. But dependencies removed from the PKGBUILD would stay installed,
and `overlay-chroot.md` already rejected "a chroot per package" for exactly
that: a leftover `-dev` package changes what `configure` detects, and a
package can link against something it never declared.

## Order

1. **Base refresh by snapshot and swap.** It removes a failure mode (a broken
   base) and a source of contention (the refresh lock), and it is small: the
   refresh path, the recovery, and the removal of the lock logic.
2. **Keep failed builds.** Small and independent, useful for debugging, and it
   brings the naming parser the refresh's recovery also uses.
3. **Discard kept trees on failure.** A few lines in the failure path. Rollback
   only if a long rebuild justifies it.
