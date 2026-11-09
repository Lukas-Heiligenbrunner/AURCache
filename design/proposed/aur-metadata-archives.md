# Design: Serving AUR Metadata from the Bulk Archives

Status: **Proposed** · Last updated: 2026-08-27

## Motivation

Every question AURCache asks about the AUR goes to the RPC, one HTTP request at
a time, from `aurcache-deps`:

| Call site | RPC endpoint | Frequency |
|---|---|---|
| `aurcache_api::aur::search` (add dialog) | `/search/<q>?by=name-desc` | one per debounced keystroke, per user |
| same route, queries under 3 chars | `/info?arg[]=<q>` | same |
| `resolve_aur_pkgbase` (add) | `/info` | once per add |
| `AurClient::provider_pkgbase` | `/search/<dep>?by=provides` | **once per unresolved dependency**, sequentially |
| `update_version_check` | `/info` (chunked) | every `version_check_interval` (default 1 h) |
| dependency backfill migration | `/info` per pkgbase | once, on upgrade |

The wiki asks bulk clients to stop doing this
([Aurweb RPC interface § AUR metadata archives](https://wiki.archlinux.org/title/Aurweb_RPC_interface#AUR_metadata_archives)),
and the RPC's own limits are ones we already run into:

- **4000 requests per day per IP.** Shared by every AURCache instance behind
  the same NAT, and search spends them one debounced keystroke at a time.
- **A search that matches 5000 or more packages fails outright.** This is not
  theoretical: `python`, `git` and `lib` all return
  `{"type":"error","error":"Too many package results."}` today, so typing any of
  them into the add dialog produces a red error rather than results. Locally
  they match 11 075, 31 509 and 12 292 packages — all rankable, none of them a
  failure.
- **URI length.** `rpc_info_urls` exists solely to chunk `/info` under the
  server's request-line limit, with a comment explaining that getting it wrong
  stops *every* package from being version-checked.
- **Latency.** A measured RPC search takes 0.5–0.7 s; the same match against a
  local index takes single-digit milliseconds.

## What the archives give us

Measured 2026-08-27:

| Archive | Compressed | Uncompressed | Contents |
|---|---|---|---|
| `packages.gz` | 548 KB | — | 118 249 pkgnames, one per line |
| `pkgbase.gz` | 515 KB | — | 111 174 pkgbases, one per line |
| `packages-meta-v1.json.gz` | 10.2 MB | 52.8 MB | every package, `type=search` fields |
| `packages-meta-ext-v1.json.gz` | 14.0 MB | 72.8 MB | every package, `type=multiinfo` fields |

All are at `https://aur.archlinux.org/<name>.gz`, regenerated on a ~5 minute
interval, and support `Last-Modified`/`ETag`.

**The conditional GET is not the saving it looks like.** A 304 does come back
zero-byte against an unchanged archive, but the archive is rebuilt every five
minutes and the ETag changes each time — measured, ten minutes apart:
`"6a906712-d5ca46"` then `"6a90696a-d5ca4e"`, and an `If-None-Match` with the
older one returned a full body, not a 304. At any cadence coarser than five
minutes the ETag has always moved on, so **refresh cadence is download
cadence**: a full archive every time we check. Conditional requests are still worth
sending — they cost one round trip and cover the case where the rebuild is
delayed — but they buy nothing we can plan around.

This design uses the *smaller* of the two metadata archives — see
"What it costs to hold" — but the larger one is worth documenting, because it
is the escape hatch if we ever need dependency lists or `Provides` locally.
`packages-meta-ext-v1.json.gz` carries every field `aurcache_deps::Package`
deserializes, for all 118 249 records: `ID`, `Name`, `PackageBaseID`,
`PackageBase`, `Version`, `Description`, `URL`, `NumVotes`, `Popularity`,
`OutOfDate`, `Maintainer`, `Submitter`, `FirstSubmitted`, `LastModified`,
`URLPath`, plus `Depends`, `MakeDepends`, `OptDepends`, `CheckDepends`,
`Provides`, `Conflicts`, `Replaces`, `Groups`, `License`, `Keywords`,
`CoMaintainers`. Nothing we read from the RPC is missing from it.

### Fidelity, checked rather than assumed

- **`by=name-desc`** is a case-insensitive substring match on `Name` or
  `Description`. Reproducing that over the archive returned *exactly* the same
  set as the live RPC for `hello` (23) and `Kdeconnect` (10) — no extra
  results, none missing.
- **`by=provides`** matched exactly for `java-runtime` (173) and `ttf-font`
  (95) once version constraints are stripped (`Provides` in the archive holds
  raw `foo=1.2` strings; `parse_dep` already splits those). One difference,
  and it matters: the RPC also returns a package whose *name* is the searched
  term — `by=provides` for `hello` returns `hello`, which no `Provides` list
  contains. **Any local implementation must union the provides match with an
  exact name match**, or `provider_pkgbase` silently stops resolving
  self-providing dependencies. Recorded because it is the trap waiting for
  whoever moves provides resolution local later — this design does not, for
  reasons the frequency section measures.
- **`/info`** is a name lookup; the archive is keyed by `Name` with
  `PackageBase` alongside, which is what every one of our `/info` callers
  actually wants.

### What it costs to hold

Measured with a release-mode Rust binary using the real `Package` field set
(gunzip + `serde_json`, RSS after dropping the raw JSON):

| Shape | Parse | Resident |
|---|---|---|
| Full `Package` (all 20+ fields), ext archive | 128 ms + 208 ms | **182 MiB** |
| Trimmed (name, base, version, description, maintainer, url, votes, popularity, out-of-date, provides), ext archive | 133 ms + 113 ms | **59 MiB** |
| Same shape minus `provides`, `packages-meta-v1`, `String` fields | 95 ms + 82 ms | **49 MiB** |
| Same, with `Box<str>` fields | 95 ms + 80 ms | **44 MiB** |

182 MiB is too much to hold on a box that is also running container builds.

**This design uses `packages-meta-v1.json.gz` and the 49 MiB shape** — the
smaller archive, with no `Provides` and no dependency lists. The frequency
measurements in the next section are why: provides resolution turns out to cost
one RPC request per ~25 packages resolved, which is not worth 7 MiB of RAM,
4 MB more per download, and the merge subtleties that an `Option<Provides>`
field drags into the overlay. Dependency lists were never needed — `add` reads
them from the `.SRCINFO` in the snapshot, not the RPC.

A pleasant side effect: `packages-meta-v1` is the `type=search` format, so an
RPC search result and an archive record have **identical field sets**, and
mixing them needs no unknown-versus-empty reasoning at all.

## How often each call actually fires

Traced through the workspace, because the answer decides which of these is
worth optimising:

| Call | Fires when | Requests per firing |
|---|---|---|
| `search_by_name` | every debounced keystroke in the add dialog, per user | 1 |
| `info_of` | same, for queries under 3 characters | 1 |
| `resolve_bases` | inside `resolve_dependency_resolutions` | 1 batched `/info` for all dep names at once |
| `provider_pkgbase` | same, per dep that reached it | 1 `/search?by=provides`, sequentially |
| `multi_info_of` | `update_version_check`, once per cycle (default hourly) | 1–2 for the whole instance |
| `deps_of` | dependency-backfill migration only | 1 per pkgbase, once ever |

`resolve_dependency_resolutions` is the one worth spelling out, because it is
not only an add-time path. It runs from `package_add`, from `package_update` —
which is reached by the retry/force-rebuild route (`build.rs:372`), the manual
update endpoint (`package.rs:447`), and `package_update_all_outdated` in *both*
the version-check and auto-update schedulers — and from
`package_resync_dependencies` after a per-package settings edit. In short: **every
time a package's graph is touched, including every build trigger.**

### What that costs, measured

Sampling 400 random AUR packages and resolving their `Depends` + `MakeDepends`
the way `resolve_dependencies` does — local repo, then official repos from the
cached pacman DBs, then exact AUR name, then a provides search — against the
real `core`/`extra`/`multilib` databases:

| Outcome for a dependency | Share | Network cost |
|---|---|---|
| Satisfied by official repos (cached DB on disk) | 88% | none |
| Exact AUR pkgname | 11% | folded into the one batched `/info` |
| Needs its own `by=provides` search | **1%** | one RPC request each |

5.4 distinct dependencies per package. Provides searches per package: **mean
0.04, median 0, p90 0, max 2** — 96% of packages need none at all.

**This corrects an earlier claim in this document.** The `provider_pkgbase`
N+1 looked like the biggest request-count win when read off the code shape:
a sequential RPC search per unresolved dependency. Measured, it is a rounding
error, because the official-repo cache absorbs seven of every eight
dependencies before the AUR is consulted. A dependency resolution costs
essentially **one batched `/info`**, whether it happens at add time or on the
hundredth rebuild.

### The bulk case: standing up a server

Per-package rates hide the setup cost. The API adds **one package per POST**
(`package_add_endpoint`), the CLI loops over the names it is given, and the
dialog adds its queue one at a time — so seeding an instance with 300 packages
is 300 adds, each paying:

- 1 `/info` to map the typed name to a pkgbase (`resolve_aur_pkgbase`), plus
- 1 batched `/info` for that package's dependency names, plus
- the same again for every AUR dependency pulled in recursively (~0.6 per
  package, from the 11% exact-AUR share above).

That is roughly **900–1000 RPC requests to seed 300 packages** — a quarter of
the 4000/day budget in one sitting, before the instance has built anything.
The same shape applies to restoring a dump (`design/implemented/db-export.md`), which
replays adds.

Of those ~1000 requests, ~98% are `/info` name lookups that a warm index
answers outright. Provides searches are the remaining ~1.5%: across the entire
AUR there are 4 396 provides-only dependency occurrences over 118 249 packages
(1 827 distinct names), so 300 packages expect **about 11**.

**So the bulk case is real, but `Provides` is not what fixes it — warming is.**
The rule written above ("name lookups never trigger a download") protects a
one-off add from pulling 10 MB, and in doing so it also stops a 300-package
seeding run from ever loading the index. That is the wrong outcome, and the fix
is to make the trigger about *volume* rather than about which caller is asking:

> **Warm-up triggers.** A search arms a background index load, as already
> described. So does **a second package add within five minutes** of the
> first. Lookups keep using the RPC while it loads.

Two adds in five minutes is not a one-off; it is someone seeding an instance,
whether by CLI loop, dump restore, or clicking through the dialog. A genuinely
single add still costs one small request and no download. A seeding run arms
the load at package #2 and, since the load takes ~2 s while each add takes
longer than that, runs against the index from roughly package #3 onward.

An earlier draft of this section used a rolling counter — warm after 50
requests in an hour. That is too late: 50 requests is already ten times the
budget this design is trying to hold to. Counting adds rather than requests
arms it an order of magnitude sooner and needs no bookkeeping.

**Memoize `provider_pkgbase`** for an hour (dep name → resolution). The
provides-only names repeat heavily — `libjpeg` appears in 200 packages,
`vdr-api` in 103, `gz-cmake` in 57 — so a themed bulk add collapses its repeats
to one request each. This is the provides win, at the cost of a `HashMap`,
without carrying 118 249 `Provides` lists in memory.

### Ask only what the known repositories cannot answer

The rule above — "a dependency miss asks nothing" — is stated negatively, and a
negative rule is easy to get wrong later. The positive form is better and is
what should actually be implemented: **resolve every dependency against the
repositories we already know about, and send only the leftovers to the AUR.**

Today the order is inverted. `resolve_dependencies` sends *all* dependency
names to `resolve_bases` up front, and only then loops over them checking the
local repo and the official repos. Moving the AUR call after that loop, and
giving it only the survivors, is a win with or without any of this archive
work:

| | Names sent to the AUR per package |
|---|---|
| Today | 5.4 (every dependency) |
| Positive check first | **0.65** |

Median 0, p90 2, max 19 — and **70% of packages need no AUR request at all**,
even with a completely cold index, because everything they depend on is in
`core`/`extra`/`multilib` or already built here.

The known-repository check already covers the case you would worry about:
anything AURCache has built lands in `<arch>/repo.db.tar.gz` and is matched by
`local_repo_dependency_exists` regardless of where its source came from, so a
git-sourced package satisfies dependents by name or by `provides` exactly as an
AUR-sourced one does. Tracked-but-not-yet-built packages are covered a layer
up, by `resolve_local_dependency_resolutions`, which matches against
`packages`, their `split_packages` and their `provides` — including packages an
in-flight add has only planned.

### There is no set — and that is the other half of the cost

`official_dependency_exists` and `local_repo_dependency_exists` do not consult
an index. Each call runs `any_archive_provides`, which opens
`core.db.tar.gz`, `extra.db.tar.gz` and `multilib.db.tar.gz` in turn,
decompresses each, and walks its `desc` entries until it matches — **once per
dependency name**, and a name that is *not* official walks all three archives
to completion before returning `false`.

`extra.db` alone holds 14 928 entries; decompression alone is ~50 ms, before
any tar walking or UTF-8 decoding. A package with 5.4 dependencies pays that
repeatedly, and the packages that pay the most are precisely the ones with
dependencies the AUR must answer for — the misses.

So the fix your question points at is the same fix twice over:

> **Build a `HashSet<String>` of names *and* unversioned `provides`, once per
> cache load, and answer every dependency check from it.** The official set measures 22 706
> entries against today's `core`/`extra`/`multilib` — on the order of 1 MiB
> resident, against ~49 MiB for the AUR index it sits beside.

**A set, not a map**, because a set is exactly what the decision consumes.
`official_dependency_exists` answers a `bool`, and a dependency that is
satisfied officially becomes `DependencyResolution::Official`, which carries no
payload and is `continue`d by every consumer (`add.rs:515`, `update.rs:436`,
and the migration) — no dependency edge, nothing to look up. A map would be
storing values with no reader.

Two things a map *could* hold, and why neither pays today:

- **Versions**, to check the `>=` constraints that `parse_dep` already
  extracts. But `Official` deps have their constraints dropped on purpose, and
  the only sensible response to an unsatisfiable one is to let the build fail —
  where pacman reports it better than we could.
- **Which package provides it**, so a local-repo match could return
  `Local { pkgbase }` instead of `Official`. Worth knowing that
  `client.rs:207` currently labels *both* the local-repo hit and the official
  hit as `Official`, losing the pkgbase. It is harmless as things stand: every
  package AURCache tracks is caught one layer up by
  `resolve_local_dependency_resolutions`, which returns a proper
  `Local { pkgbase }`, so the file-based check only ever sees untracked
  leftovers — packages still sitting in the repo that nothing can cascade a
  rebuild to. If that ever stops being true, this is the map to build, and this
  paragraph is why.

**Use `String`**, here and in the AUR index, and revisit only if memory turns
out to be a constraint. `Box<str>` was measured, not guessed: the same 118 249
records parse to **49 MiB with `String` fields and 44 MiB with `Box<str>`** — a
saving of ~5 MiB, or 10%, at identical parse time. That is a trim, not a
transformation, and it is not worth pre-committing the whole codebase to a less
familiar type for. The measurement is recorded here so the option can be taken
later without re-deriving it; `serde` deserializes `Box<str>` directly and
`Borrow<str>` still gives `contains(&str)`, so the change stays mechanical
whenever someone wants those 5 MiB.

A structural idea that looked more promising did *not* pay either:
`PackageBase` equals `Name` for **110 215 of 118 249 records (93%)**, so
storing it as `Option<String>` should drop 110 000 small allocations. Measured,
resident size did not move — glibc's allocator keeps freed blocks in its arena
rather than returning them to the OS, so the ~3 MiB is real in principle and
invisible in practice. Not worth the extra field semantics.

Invalidation is the only real design point, and both sides already have a
signal: the official set is rebuilt when the cached `.db.tar.gz` files are
refreshed (they carry a 1-hour TTL and their mtimes are already the staleness
check), and the local-repo set is rebuilt when `repo.db.tar.gz`'s mtime moves,
which is exactly when a build has published a package.

This is independent of the AUR archives and worth doing on its own: it turns
the dependency path from *N* tarball scans per package into one hash lookup per
dependency, and it makes the "positive check first" ordering free, which is
what lets the AUR request list shrink to 0.65 names.

**And the miss path needs no special case.** A dependency name that is in none
of the known repositories and not in the AUR index falls through to
`provider_pkgbase`, whose RPC `by=provides` search returns exact name matches
as well as provides matches — verified earlier: `by=provides` for `hello`
returns `hello`, which no `Provides` list contains. So a genuinely
just-published AUR dependency still resolves, in one request, exactly as it
does today. The negative rule of the previous section is subsumed: nothing
needs to "fall back on a miss", because the provides search *is* the fallback.

### Holding it to a dozen requests an hour

That is the target, and it is reachable — but only with one correction to the
fallback rule stated earlier, which as written would have made things *worse*
than today.

**A dependency-name miss must not fall back to the RPC** — restated from the
section above, because it is the rule most likely to be "fixed" back into a
bug. With the positive check first, only 0.65 names per package reach the AUR
at all; if a miss on those re-queried the RPC, the 88% that the known
repositories answer would come straight back as requests. The distinction:

| Lookup | On a miss |
|---|---|
| `resolve_aur_pkgbase` — the name a *user typed* | RPC `/info`. A miss plausibly means "published in the last few minutes" |
| `resolve_bases` — *dependency* names | Nothing. A miss means "not an AUR package", which is the expected answer for seven of every eight dependencies, and the existing flow already handles it by falling through to the official and provides checks |

With that fixed, seeding 100 packages against a warm index costs:

| | Requests |
|---|---|
| Name → pkgbase for 100 typed names | **0** — all in the index |
| Dependency resolution for 100 packages | **0** — 70% of packages send nothing at all, the rest are answered by the index |
| Provides searches (~4 expected per 100 packages, memoized) | **≤4** |
| Warm-up: adds #1 and #2 before the index lands | **~4** |
| Version check, meanwhile | **1–2** |
| **Total** | **~10** |

Against roughly 1000 today. The archive download itself is one request for a
static file — which is precisely the trade the AUR asks bulk clients to make,
and it is not counted against the RPC's 4000/day limit.

Two things this does *not* do, deliberately. It is not a hard cap: refusing to
make a request would turn "add a package published ten minutes ago" into a
failure, which is a worse outcome than the request. And it does not batch adds
into one API call — worth doing on its own merits, but it would not change
these numbers, because the per-add cost is already zero.

What it should do is **notice**: count AUR RPC requests per hour and log a
warning when the count exceeds `AUR_RPC_BUDGET_WARN` (default 100). If some
future change reintroduces a per-package request, the number is the thing that
tells us, rather than a user hitting the AUR's rate limit.

**Which leaves the ext archive still not paying.** Against a seeding run:
warming and the miss rule remove essentially every `/info`, memoization removes
most of the provides searches, and switching from `packages-meta-v1` to
`packages-meta-ext-v1` would save what remains — a handful of requests — for
4 MB more on every refresh and 7 MiB more resident, plus the absent-versus-empty
merge rule. The recommendation stays with the smaller archive, and the open
question at the end records the conditions that would flip it.

So the archive's value is concentrated almost entirely in **search**: the
volume (per keystroke, per user), the outright failures on broad queries, and
the 0.5–0.7 s latency. Everything else was already cheap.

## How stale is too stale

There is no single right refresh interval, because the three things we would
read out of the archive tolerate wildly different staleness. Measured from the
archive's own `FirstSubmitted`/`LastModified` timestamps on 2026-08-27:

| Window | Packages newly submitted | Packages modified |
|---|---|---|
| 1 hour | 0 | 50 |
| 6 hours | 19 | 377 |
| 24 hours | 53 | 1 062 |
| 7 days | 476 | 4 472 |
| 30 days | 1 803 | 11 642 |

**Missing a recently-added package is the cheap risk.** About 53 packages a day
are submitted, so a 24-hour-old cache is unaware of at most ~53 names out of
118 249 — 0.045% of the AUR, and only for someone searching for a package
submitted since the last refresh. It is also the risk the fallback already
covers: the add dialog takes a name as typed without requiring a search result
to be clicked, and an exact-name lookup that misses the archive falls through
to the RPC. Someone adding a package submitted an hour ago still gets it; they
just do not see it in the dropdown first.

**Missing a version bump is the expensive risk.** ~1 062 packages are modified
per day, ~44 per hour — roughly 0.9% of the AUR daily. For an instance tracking
50 packages that is about one package a day whose update exists upstream but
not in a day-old archive. Serving version checks from a 24-hour-old archive
would turn "out of date within the hour" into "out of date within a day and a
bit", which is the one thing AURCache is for.

So the answer is not one interval. Each caller states the staleness it
tolerates, the archive is refreshed only when some caller's demand is not met
by the cached copy, and the caller with the tightest tolerance — the version
check — is the one that keeps using the RPC instead.

| Caller | Max age it accepts | If the cached copy is older, or absent |
|---|---|---|
| `search_by_name` | 24 h (`AUR_ARCHIVE_MAX_AGE`) | **Answer from the stale index now**, arm a background refresh. Cold: answer from the RPC this once |
| `provider_pkgbase` | — | Not an index user; stays on the RPC, with an in-process memo of its results |
| `info_of` / `resolve_bases` / `multi_info_of` | any age, **plus RPC fallback for a missed user-typed name** | A cold index goes to the RPC; a second add within five minutes arms the warm-up |
| `update_version_check` | its own `version_check_interval` (default 1 h) | **Never triggers a download — batched RPC `/info`, as today** |

**Search pays for the archive; the pkgbase lookups ride it, and sustained bulk
work pays for it too.** Search is the one caller with no cheap targeted RPC
equivalent — a substring query cannot
be narrowed to a few names, and broad ones fail outright today — so it is the
caller that justifies the download. The name lookups have a small precise RPC
call available, so they read the index when it happens to be there and fall
back when it is not. A headless CLI-driven instance that never searches
therefore behaves exactly as it does today, at today's request count, and never
downloads the archive at all.

In practice the ride is rarely free-but-unused: the normal add flow *is* search
→ click → add, so the index is warm precisely when the pkgbase lookups run.

The practical result on a default configuration: the archive is downloaded at
most once a day, and only if somebody actually searched; the version check
keeps costing 1–2 small RPC requests an hour and keeps detecting updates within
the hour. Neither number is worse than today, and the request count for
searches and provides-resolution goes to zero.

An operator who wants version checks off the RPC entirely can raise
`version_check_interval` to a day, or lower `AUR_ARCHIVE_MAX_AGE` below it —
either way the rule above starts serving version checks from the archive,
because the freshness demand is then met. That is the knob, and it is one the
existing setting already expresses.

### A search does not refresh the archive

Max-age is a **staleness ceiling, not a schedule**, and refresh is lazy. A
search consults the in-memory index and returns; it touches the network only if
the cached copy has aged past the ceiling, which on a 24-hour default means at
most one download a day, paid by whichever search happens to be the first one
after expiry. Every other search that day — however many people type into the
add dialog, however many keystrokes each — costs nothing off the AUR and
answers in single-digit milliseconds locally.

The pathological reading, "each add-package search pulls 10 MB", is exactly what
the max-age is there to prevent. The one path that could still surprise us is a
*failing* refresh being retried by every subsequent search, so a failed refresh
records the attempt time and is not retried for five minutes; searches in that
window answer from the stale index rather than hammering a service that is
already having a bad day.

### A lookup never waits for the network

Measured against the real host: **1.6–1.9 s** to download the 10.2 MB
`packages-meta-v1.json.gz` (0.5 s of it time-to-first-byte), then ~95 ms to
gunzip and ~79 ms to parse, plus index building. Call it ~1.9 s on a good
connection and rather worse on a domestic one — 10 s at 1 MB/s.

That must never land on a search. The rule:

| Index state | What the search does |
|---|---|
| Present and fresh | Answers locally, single-digit ms |
| Present but past max-age | **Answers from the stale index immediately**, arms a single-flight background refresh; the next search gets the new data |
| Absent (cold) | Answers this one query from the RPC — today's ~0.5 s — and arms the load in the background |

Serving stale data while revalidating is not a compromise here: an index that is
*allowed* to be 24 hours old is self-evidently allowed to be 24 hours and two
seconds old. Blocking a keystroke on a 10 MB download to avoid two extra
seconds of staleness would be the wrong trade by three orders of magnitude.

**Startup pre-warm.** If an on-disk copy exists, parse it into memory in a
background task at startup — ~250 ms, paid by nobody, and it closes the cold
window before the first user arrives. Startup does *not* download: an instance
nobody searches on should fetch nothing, and a boot should not cost 10 MB.

### Nothing on the build path reads the archive

Worth stating plainly, because it is what makes a long max-age safe: adding a
package resolves the pkgbase, then downloads the **live AUR snapshot** and
reads its `.SRCINFO`. The stored pkgbase, version, split names, provides and
dependency graph all come from `store.sourceinfo(...)` — the current git tree —
not from the RPC and not from the archive. The archive's only job in that flow
is mapping a typed pkgname to a pkgbase.

Two consequences:

1. **"Add it and build it now" is never blocked by a stale index.** The build
   is queued off the snapshot, which is fetched fresh at add time. A day-old
   archive cannot cause an old version to be built.
2. **A stale mapping fails loudly, not silently.** If a package was renamed or
   moved between pkgbases since the last refresh, the snapshot download 404s
   and the add errors out. It does not quietly build the wrong thing.

### The brand-new package, and what actually fixes it

The case that motivates all of this: someone publishes an AUR package, comes
straight to AURCache to build it, searches, and does not find it. Three
sources, three different lags:

| Source | Lag on a just-published package |
|---|---|
| RPC | none worth counting — served from aurweb's live database |
| Archive, freshly downloaded | up to ~5 minutes (measured rebuild boundaries: 16:34:27, 16:44:27, 16:49:27, 16:54:27 — 5-minute aligned) |
| Archive, at a 24 h max-age | up to 24 hours |

This is what settles the "refresh the archive vs. use the RPC for this one
call" question, and it settles it against refreshing:

- **Refreshing costs 10 MB to answer one query**, and there is no
  conditional-GET relief because the ETag has always moved on.
- **Refreshing may not even answer it.** A package published 90 seconds ago is
  not in the archive that a refresh would download, because the archive itself
  is a 5-minute batch. The user would click Refresh, wait, and still not find
  their package — the worst possible outcome for an escape hatch.
- **A single live RPC search costs a few KB, answers in ~0.5 s, and is the
  freshest source that exists.** It is also precisely scoped to the query the
  user is asking about.

So the user-facing escape hatch is a one-off live search, not a refresh.

**Surfacing it.** A single muted line directly *below* the results box, always
rendered whenever results came from the index, stating the data's age and
ending in a link-styled action:

```
┌ Add package ──────────────────────────┐
│  ┌──────────────────────────────────┐ │
│  │ myapp                            │ │
│  └──────────────────────────────────┘ │
│  ┌──────────────────────────────────┐ │
│  │ myapp-bin            1.2.0-1     │ │
│  │ myapp-git            1.2.r4-1    │ │
│  │ myapp-docs           1.1.0-2     │ │
│  └──────────────────────────────────┘ │
│  AUR data from 14:03 · Search live    │  ← text-xs opacity-50
│                                       │
│                  [ Cancel ]  [ Add ]  │
└───────────────────────────────────────┘
```

`text-xs opacity-50` for the line, the action as a `link link-hover` button
rather than a `btn` — it should read as a footnote, not as a second call to
action next to **Add**.

Its states:

| Situation | Line reads |
|---|---|
| Index results, any count | *AUR data from 14:03 · **Search live*** |
| Live search in flight | *Searching the AUR…* (action disabled) |
| Live results shown | *Live AUR results · just now* (no action — it just ran) |
| Live search failed | *Live search failed: Too many package results. Showing AUR data from 14:03.* — **local results stay on screen** |

Three things this placement gets right:

- **It is not conditional on the result count.** Anchoring it to the empty
  state would miss the most likely version of this case: a new package whose
  name is a superset of existing ones. Publishing `myapp` when `myapp-bin` and
  `myapp-git` exist gives a *non-empty* list that still does not contain what
  the user came for, and an empty-state-only affordance would never appear.
- **It is outside the scrollable list.** The results box is
  `max-h-56 overflow-y-auto`, so anything appended as a final row — a "not
  here?" entry — is scrolled out of reach exactly when the list is long. Below
  the box it is always on screen.
- **It sits where the eye already is** when the list disappoints, without
  competing with the results or the Add button.

Deliberately *not* done: an icon-only button in the search field (invisible to
touch users, hides the age behind a hover), and revealing the action only for
query strings that look like bare package names (an invisible rule that makes
the link appear and vanish mid-typing, reading as a glitch).

This needs the search response to carry its own provenance, so
`GET /search` gains a `live=true` parameter and returns an envelope
(`{ source: "archive" | "rpc", generated_at, results }`) instead of a bare
`Vec<ApiPackage>`. That is a breaking change to the route, but its only
consumers are `aurcache-client`, `aurcache-cli` and `frontend-rs`, all in this
repo and all updated in the same change.

A live search still inherits the RPC's limits — a query matching ≥5000
packages fails. Since the archive already answered that query locally, the
failure is reported as "the live search failed" *next to* the local results,
never as a replacement for them.

Keeping the affordance this quiet is safe because **nobody is blocked by not
finding it**. The dialog already accepts a name as typed without requiring a
search result to be clicked, and the add path resolves an unknown name over the
RPC through the exact-name fallback. Someone who publishes a package and
immediately types its name into AURCache gets it built whether or not they ever
notice the live-search link; the link only saves them from doubting that it
will work.

**Manual whole-archive refresh stays**, but as an operator tool rather than a
dialog button: `POST /aur/refresh` plus `aurcache-cli aur refresh`, reporting
the new index's age and record count. It is single-flight — one download, not
one per caller — and floored at five minutes, because refreshing against a copy
younger than the rebuild interval cannot return anything newer.

### The index is a cache, not a snapshot

Every RPC response we receive is newer than the archive that produced the
index, so it should be kept rather than thrown away. The archive is the bulk
baseline; the RPC calls we still make patch it as they go:

| RPC call we already make | What it teaches the index |
|---|---|
| `update_version_check`'s batched `/info` (hourly) | current `Version` and `OutOfDate` for every tracked package |
| A one-off live search | the packages matching that query, including ones the archive has never heard of |
| Exact-name fallback in `resolve_bases` / `info_of` | the freshly-published package the user is adding |

The effect is that the packages a given instance actually cares about stay
hourly-fresh regardless of the archive's max-age, and a package that has ever
been looked up live is in the index from then on. It also makes the daily
default much easier to defend: the stale 99% is the long tail nobody on this
instance has touched.

**Shape.** The parsed archive stays immutable behind an `Arc` — it is the
expensive part and must remain cheaply swappable. Updates land in a small
`RwLock<HashMap<String, OverlayRecord>>` consulted before the archive index,
where each `OverlayRecord` carries the record and the instant it was fetched.
The overlay is bounded by how many packages an instance touches, which is
orders of magnitude below 118 249.

Two details that decide whether this is correct or subtly wrong:

- **Survive the refresh, but only where newer.** After an archive refresh,
  overlay entries older than the new archive's `Last-Modified` are dropped —
  the archive now says the same thing or better. Newer entries stay.
- **Additions and updates only; never deletions.** An RPC search returning
  nothing does not prove a package is gone, so nothing is negatively cached.
  Removals reach the index only through an archive refresh. The
  `aur_missing` flag stays where it is today — driven by the version check's
  own response, not by an index lookup.

Both `type=search` and `type=multiinfo` responses are supersets of the record
shape we keep, so an overlay write is a plain replace — no field-level merge,
which is exactly the subtlety the smaller archive bought us out of.

With the overlay in place, the archive's max-age is genuinely about the long
tail, which is why 24 hours is comfortable and why a week would not be absurd.

## Design

### One cache, inside `aurcache-deps`

A new `aurcache-deps/src/archive.rs` alongside `repo.rs`, following the
existing official-repo cache precedent: a file on disk in a persisted
directory, a max-age, and a lazy refresh — plus the RPC-fed overlay, which the
official-repo cache has no equivalent of.

```
AurIndex
  ├── on disk:  <cache dir>/packages-meta-v1.json.gz   (+ .etag sidecar)
  ├── archive:  Arc<ArchiveIndex>              (built lazily, swapped atomically)
  └── overlay:  RwLock<HashMap<String, OverlayRecord>>     (RPC-fed, small)

ArchiveIndex                              ArchiveRecord (trimmed, ~49 MiB total)
  ├── generated_at: SystemTime              ├── name, package_base, version
  ├── records:  Vec<ArchiveRecord>          ├── description, maintainer, url
  └── by_name:  HashMap<String, u32>        └── num_votes, popularity, out_of_date

OverlayRecord = { record: ArchiveRecord, fetched_at: SystemTime }
```

A lookup consults the overlay first, then the archive. Both hold the same
record shape, so that is a lookup and a fallthrough — no field-level merge.

- Path resolves through `aurcache_deps::paths`, as `mirrorlist_dir()` /
  `aur_archive_cache`, overridable with `AUR_ARCHIVE_CACHE_DIR` — same shape as
  `official_repo_cache_dir()`, so it lands in the same persisted volume and
  survives a restart.
- Base URL from `AUR_ARCHIVE_URL`, defaulting to the RPC URL with `/rpc/v5`
  stripped (`snapshot_url` already derives a base that way — reuse it, do not
  re-derive it).
- Refresh is a conditional GET (`If-None-Match` from the sidecar), which will
  almost always come back 200 with a full body — see above.
- Refresh is **lazy and caller-driven**: nothing polls. A lookup refreshes only
  if the cached copy is older than the *max-age that particular caller asked
  for*, so an instance nobody searches on never downloads anything.
- The overlay is memory-only. It is cheap to rebuild — the next version check
  repopulates every tracked package within the hour — and persisting it would
  mean inventing an eviction and versioning story for a cache that exists to be
  temporary.

### The `AurClient` surface does not change

`search_by_name`, `info_of`, `multi_info_of`, `resolve_bases`,
`provider_pkgbase` and `deps_of` keep their signatures. Whether an answer came
from the archive or the RPC is internal, so no call site outside
`aurcache-deps` moves, and the RPC path stays as the fallback rather than being
deleted.

### Per-call-site policy

Every row that talks to the RPC also **writes what it learns back into the
index overlay**, per the section above; that is not repeated in each row.

| Call | New behaviour |
|---|---|
| `search_by_name` | Index, never blocking (see the latency rule above) — **the only caller that triggers a download**. A search is a bulk query — exactly what the archives exist for — and the RPC's 5000-result failure disappears. Going live is the user's explicit choice (`live=true`), not an automatic fallback. If the archive cannot be loaded at all, fall back to the RPC so search degrades to today's behaviour rather than breaking. |
| `info_of` / `resolve_bases` / `multi_info_of` | Index first. **A miss falls back to the RPC only for a user-typed package name**, never for dependency names — see the dozen-requests section, where that distinction is the difference between zero requests per add and one per official dependency. |
| `provider_pkgbase` | **Stays on the RPC**, plus an in-process memo (dep name → resolution, 1 h TTL) so a bulk add does not re-ask for `libjpeg` 200 times. Measured at ~1% of dependencies and a mean of 0.04 requests per package resolved, it is not worth carrying `Provides` in the index. |
| `deps_of` (backfill migration) | Unchanged. It runs once, per pkgbase, on upgrade — but see the note below: it is the one caller that wants dep lists, and it can read them out of the ext archive if the index is already warm. |
| `update_version_check` | **Archive only if the cached copy is younger than `version_check_interval`; otherwise the batched RPC `/info`, as today.** Never triggers an archive download itself — but its response is the single richest thing the overlay gets, refreshing every tracked package hourly. See below. |

### Why the version check does not force an archive download

Being honest about the trade: today's version check is 1–2 RPC requests per
hour returning a few hundred KB. Making it archive-fed at the same detection
latency would mean a 10 MB download every hour — about 240 MB/day, and, since
the ETag always moves, no conditional-GET relief. That is *more* bytes off the
AUR than we take today, to answer a question the RPC answers well in two
batched requests.

The archives win on *request count* and on the queries that are genuinely bulk
or genuinely failing, not on this one. So the archive is justified by search
and by `provider_pkgbase`, and the version check **rides along for free when a
copy fresh enough for it happens to be on disk** — which, with the max-ages
above, it usually will not be. That is the correct outcome, not a compromise:
it keeps a build-only instance's traffic exactly where it is today.

Two things get simpler when the version check does use the archive:

- The split-package workaround in `check_versions` — querying by the first
  split child because `/info` matches pkgname, not pkgbase — goes away. The
  archive is indexed by both.
- `rpc_info_urls` chunking stops applying to that path entirely.

And one thing gets more dangerous: `aur_missing` is set from "not in the
response". A failed or empty archive load must **never** be treated as "every
package was removed from the AUR". The archive path must return a hard error
that skips the check, exactly as an RPC failure does now.

## Search, once it is local

`query_aur` currently returns every RPC result sorted by popularity, and
`rank_results` in `frontend-rs` re-ranks the page by how well the name matches.
With 31 509 hits for `git`, the server can no longer return everything.

Ranking moves server-side, using the same rule the frontend already uses so the
two cannot disagree: exact name, then prefix, then name substring, then
description-only match; ties broken by popularity. Cap the response at 200.
`rank_results` stays where it is — it becomes a no-op reordering of an
already-ordered page, which is harmless and keeps the frontend honest if it is
ever pointed at a different server.

The `< 3 characters` special case in `aurcache_api::aur::search` can stay as is;
it is about not flooding the user with substring matches, not about the RPC's
2-character minimum.

## Settings

Environment-only, in the `aurcache-deps` style (`AUR_RPC_URL`,
`OFFICIAL_REPO_CACHE_DIR`), rather than `Setting` entries — the archive is an
implementation detail of how AURCache talks to the AUR, and nothing about it is
per-package:

| Var | Default | Meaning |
|---|---|---|
| `AUR_ARCHIVE_URL` | derived from `AUR_RPC_URL` | Base URL the archives are fetched from |
| `AUR_ARCHIVE_CACHE_DIR` | `<mirrorlist dir>/aur_archive_cache` | Where the `.gz` and its ETag live |
| `AUR_ARCHIVE_MAX_AGE` | `86400` | Staleness ceiling: how old a cached archive may get before the *next* lookup refreshes it. Not a schedule — at most one download per window, and none if nobody searches |
| `AUR_RPC_BUDGET_WARN` | `100` | Log a warning if AUR RPC requests in a rolling hour exceed this. Observability, not enforcement |
| `AUR_ARCHIVE_DISABLE` | unset | Force every call back onto the RPC |

`AUR_ARCHIVE_DISABLE` is what the e2e suite and any air-gapped mirror setup use,
and it is the escape hatch if the archive format ever changes under us.

## Testing

- **Unit, in `aurcache-deps`**: a small hand-written fixture archive (a dozen
  records) covering name-desc matching, ranking order, and the 200-cap.
- **Fidelity, `#[ignore]`d like `live_rpc_chunking.rs`**: fetch the real archive
  and the real RPC for a handful of queries and assert the result *sets* are
  equal. This is the test that catches the archive schema drifting.
- **Refresh policy**: a `wiremock` server asserting that a lookup inside the
  max-age makes no request; that an expired one sends `If-None-Match` and
  rebuilds the index on a 200; that a 304 keeps the existing index; and — the
  one that protects the AUR from us — that a version check **never** triggers a
  download, only ever reading a copy that is already fresh enough for it.
- **Failure**: archive fetch fails → search falls back to RPC; archive fetch
  fails → version check errors out rather than flagging every package
  `aur_missing`; a failed refresh is not retried by the next search within five
  minutes.
- **Manual refresh**: two concurrent `POST /aur/refresh` calls produce one
  download; a refresh against a copy younger than five minutes downloads
  nothing and still reports the age.
- **Known-package sets**: a dependency satisfied by `extra` is resolved without
  touching the AUR; one satisfied by a *git-sourced* package already in the
  local repo likewise; the set is rebuilt when `repo.db.tar.gz`'s mtime moves,
  so a package that has just finished building satisfies the next resolution.
- **Request budget**: the headline claim, asserted rather than hoped for — a
  simulated 100-package seeding run against a `wiremock` AUR makes fewer than
  a dozen RPC requests in total, and exactly one archive fetch. Sub-cases: a
  single add arms no load; a second add within five minutes arms exactly one;
  resolving `libjpeg` across twenty packages issues one provides search; and
  **a resolution whose dependencies are all official-repo packages makes zero
  requests**, which is the regression that would otherwise reappear the moment
  someone re-reads the fallback rule and applies it to dependency names.
- **Never blocks**: with a `wiremock` archive that never responds, a search
  against a stale index still returns promptly from the stale data, and a
  search against a cold index returns promptly from the RPC. This is the test
  that stops someone "simplifying" the background refresh into an `await`.
- **Overlay**: an `/info` response makes a previously-unknown package findable;
  an archive refresh evicts overlay entries older than the new archive and
  keeps newer ones; a live search that finds nothing caches no negative.
- **Add dialog, rendered**: the provenance line appears with a *non-empty*
  result set (the `myapp` / `myapp-bin` case, which an empty-state-only
  affordance would miss), the failed-live-search state keeps the local results
  on screen, and the action disappears once live results are shown.
- `./scripts/test-frontend.sh` for the add dialog, since the search response
  shape and ordering change.

## Staging

1. `archive.rs`: fetch, cache, conditional refresh, trimmed index. No callers.
2. `search_by_name` onto the index, with server-side ranking and the cap.
   Fixes the `python`/`git`/`lib` failure. Ships together with the `live=true`
   parameter, the response envelope, and the provenance line plus *"Search the
   AUR directly"* action in the add dialog — the escape hatch is what makes a
   24-hour default acceptable, so it cannot land later than the staleness it
   compensates for.
3. The known-package sets and the positive-check-first ordering. Independent
   of the archives, and the largest single reduction in AUR requests measured
   anywhere in this document: 5.4 names per package down to 0.65, with 70% of
   packages asking nothing.
4. Warm-up on a second add, the `provider_pkgbase` memo, and the
   dependency-name miss rule. Small, and it is what makes seeding an instance
   cheap; worth landing before anyone migrates a real package set onto this.
5. The overlay: RPC responses patch the index, with the absent-is-not-empty
   and refresh-eviction rules. Small, and it is what keeps the touched subset
   fresh regardless of max-age.
6. `info_of` / `resolve_bases` / `multi_info_of` index-first with RPC
   fallback for misses.
7. Version check reads a fresh-enough index when there is one, and writes its
   response into the overlay either way; drop the split-child query workaround.
8. `POST /aur/refresh` and `aurcache-cli aur refresh` for operators.

Each step is independently shippable and independently revertable.

## Open questions

- **Idle eviction.** 49 MiB held forever on an instance that was searched once.
  Dropping the index after, say, an hour idle is easy with the `Arc` swap, but
  it trades RAM for a re-parse. Probably not worth it until someone complains.
- **`deps_of` for the backfill migration.** Reading dep lists out of the ext
  archive would let the migration resolve the whole graph from one download
  instead of one request per pkgbase — attractive for a large instance, but it
  needs the untrimmed record shape (182 MiB) unless the migration streams the
  archive rather than indexing it. Streaming is the right answer if we do it.
- **When would the ext archive earn its place?** `packages-meta-ext-v1.json.gz`
  costs 4 MB more per download and 7 MiB more resident, and buys local
  `Provides` (and dep lists, at 182 MiB for the full shape). At today's
  measured 0.04 provides-searches per package resolved, it does not pay. It
  would if the official-repo cache were ever unavailable — that cache is what
  absorbs 88% of dependencies — or if the backfill migration moved to reading
  dep lists from the archive.
- **`packages.gz` / `pkgbase.gz`.** At ~0.5 MB they are a tempting cheap
  existence check, but nothing we do needs *only* existence — every caller
  wants the pkgbase or the version too. Left unused.
- **Should the live search be automatic rather than a click?** An empty result
  set could go to the RPC by itself. Against: most empty result sets are
  typos, so it puts an RPC call back on the keystroke path for exactly the
  queries that deserve it least, and it spends the 4000/day budget without the
  user asking. The click is one interaction in a rare case, and it makes the
  "this is live data" labelling honest. Leaning: keep it explicit, revisit if
  users report not finding the button.
- **How fresh is the RPC itself?** It reads aurweb's live database, so a
  just-published package should appear immediately, but aurweb can be
  configured with a short-lived response cache in front of the RPC. Not
  something we can measure from outside, and it does not change the design —
  the RPC is the freshest source available to us either way.
