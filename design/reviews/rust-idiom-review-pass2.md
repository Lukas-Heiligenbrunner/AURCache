# Rust review — pass 2 (2026-08-20)

Second full read of `backend/` (~140 files, ~24k lines), file by file, focused on
readability and idiom rather than lint counts. The first pass
(`rust-idiom-review.md`) was clippy-driven; this one is a reading pass, so most
of what it found is invisible to clippy.

State after the pass: `cargo fmt --check` clean, `cargo clippy --workspace
--all-targets -- -D warnings` clean, `cargo test --workspace --no-fail-fast`
fully green (see "Fixed since the first draft" §3 for the three `snapshot::tests`
that used to fail outside the builder image).

No lint was disabled and no `#[allow]`/`#[expect]` was added.

---

## Fixed since the first draft

These three were flagged for a decision; all are now implemented and covered by
tests.

### 1. Mirror ranking selected the *slowest* mirrors

`pacman-mirrors/src/benchmark.rs`

`Benchmark::measure_duration` was documented as "time in seconds to … retrieve
core.db" but returned a **transfer rate** (`size / elapsed / 1024`, higher =
better). `rank()` sorted ascending and only the first ten reached the
mirrorlist, so the generated list held the ten *worst* responders. A second bug
in the same function sampled `start.elapsed()` before awaiting the response
body, making the divisor time-to-headers rather than time-to-transfer.

Resolution: return the duration. Every mirror is measured against the same file,
so elapsed time is directly comparable and ordering by it is equivalent to
ordering by throughput — without the division, the unit ambiguity, or the
mismatch with the trait's documented contract. It also fixes the second bug
structurally: you cannot produce a duration without awaiting the body. The
method now returns `Duration` (so `sort_by_key` replaces `total_cmp`) and takes
`&self`; the throughput is still logged for operators.

*Adjacent panic:* `gen_mirrorlist` sliced `&mirrors[..10]`, panicking whenever
fewer than ten mirrors ranked successfully — the normal case. Now `.take(10)`,
with regression tests for both the short list and the cap.

### 2. `BuildLogger` leaked one tokio task per log request

`aurcache-utils/src/build_logger.rs`, `aurcache-api/src/worker.rs`

`BuildLogger::new` spawned a flush task with an unconditional `loop` that never
terminated; `Drop` spawned a final flush but did not stop it, and fired on every
clone. `job_logs` built a fresh `BuildLogger` per log-append request, so the
process accumulated an immortal task per request.

Resolution: state moved into an `Arc<Shared>` held by both the handles and the
task, plus a separate `Arc<ShutdownOnDrop>` held **only** by handles. The task
cannot keep that guard alive, so the guard's `Drop` runs exactly when the last
handle goes; it sets a flag and wakes the task, which drains the buffer and
returns. `notify_one` leaves a permit behind when the task is not parked, so the
wakeup cannot be missed whatever the task is doing.

`Drop for BuildLogger` is gone entirely — which also removes a `tokio::spawn`
during drop, an operation that panics during runtime shutdown.

Separately, callers that already batch their own output no longer pay for a task
at all: a free `append_build_output(db, build_id, text)` does the single
`UPDATE` directly, and the two `aurcache-api` sites that were only appending one
pre-batched chunk (`job_logs`, the failure-reason line in `complete_job`) use
it. The buffered `BuildLogger` remains for `ingest_pkgs`, which genuinely emits
many short lines.

### 3. Worker source tarballs carried the whole `.git` directory

`aurcache-utils/src/snapshot.rs`

`create_archive_with_pkgbase_dir` used `append_dir_all` over the *persistent git
checkout*, so for AUR sources (`subfolder == ""`) the tarball served by
`GET /worker/jobs/{id}/source` contained `.git` and the full history — on every
job download.

Resolution: the archive builder now walks the top level and skips `.git`
(sufficient: the checkout is a plain clone with no submodules). The redundant
`.git/` filter in `list_files_in_archive` is removed — having the same rule in
two places is what let the bug hide behind a clean-looking file listing, so the
archive is now the single source of truth.

Covered by `fetched_archive_excludes_git_metadata`, which drives the real path
(resolve source → clone into the persistent checkout → build the archive) and
asserts on the raw tar entries rather than through the filtering listing helper.
It first asserts the fixture checkout *does* have a `.git` dir, so it cannot
pass for the wrong reason.

*Bonus:* the three `snapshot::tests` that had been failing in every non-builder
environment were bridge-dependent (applying a patch regenerates `.SRCINFO` via
`alpm-pkgbuild-bridge`) and simply missing the `pkgbuild_bridge_available()`
guard this file already defines for that case. With the guard added,
`cargo test --workspace` is green for the first time.

## Still open

Only the four larger refactors remain; §4, §6, §7 and §11–§16 are done and are
listed under "Applied" below.

### 5. Build-status constants exist in six places

* `aurcache_common::builder::BuildStates`
* `aurcache_db::helpers::build_enqueue` (`ACTIVE_BUILD_STATUS`, …)
* `aurcache_db::helpers::dependency_resolution` (again)
* `aurcache_db::helpers::worker_jobs` (`STATUS_ACTIVE`, …)
* `aurcache_db::migration::m20260508_…` (again)
* `aurcache_utils::worker_complete` (again)

plus worker-status strings twice (`aurcache_common::worker::WorkerStatus` and
`aurcache_db::helpers::worker_store::STATUS_*`). The root cause is that
`aurcache-common` depends on `aurcache-db`, so `aurcache-db` cannot use the
canonical copy. Inverting that (put the status types in a leaf crate, or in
`aurcache-db` and re-export from `aurcache-common`) collapses all of them.

Related: `BuildStates` is a namespace of `i32` consts rather than a
`#[repr(i32)]` enum with `TryFrom<i32>`. Every read of `builds.status` is a bare
integer comparison, which is how `aurcache-cli::build_status_label` ended up with
no arm for state `4` (`WAITING_FOR_DEPS`).

### 8 + 10. `ingest_pkgs_in` is 138 lines and keeps parallel vectors

`aurcache-utils/src/repo_ingest.rs` builds `file_infos` and `build_pkgs`
separately and later `zip`s them, relying on an unstated same-order/same-length
invariant. Carrying the bytes in `FileInfo` removes the invariant and most of
the length. One refactor covers both findings.

### 9. Artifact ingest is fully in-memory

`aurcache-api/src/worker.rs::read_staging` reads every staged artifact into a
`Vec<u8>` (upload cap is 2 GiB each) and `ingest_pkgs` takes
`Vec<(String, Vec<u8>)>`. A split package with several large artifacts holds all
of them resident at once. Streaming from the staging directory would fix it.

### 10. Other functions over `too_many_lines`

* `utils/package/add.rs::insert_package_with_deps` (115)
* `scheduler/update_version_check.rs::check_versions` (114) — the `Aur` and `Git`
  arms duplicate the "compute `is_outdated`, then sync VCS sources" logic.
* `api/package.rs::get_package` (102)
* the 153-line one-shot dependency migration (reasonably exempt)

## Applied

Grouped; every change compiles, is `fmt`/`clippy` clean, and tests pass.

### Second round (§4, §6, §7, §11–§16)

* **§13 — lint-suppression policy.** `allow` is fine here; the "prefer
  `#[expect]`" sentence is gone from `backend/Cargo.toml`. The six existing
  `#[allow]`s stay as they are.
* **§14 — HTTP status codes.** A shared `utils::error::{ApiError, err}` replaces
  the ad-hoc `NotFound<String>` / `BadRequest<String>` / `Custom<String>` mix
  across `build.rs`, `stats.rs`, `activity.rs`, `settings.rs`, `package.rs` and
  `worker.rs`, so the status is now chosen explicitly at each failure site. A
  failed query is a `500`; `404` is reserved for a row that genuinely is not
  there. `setting_patch` no longer answers a store failure with `400` while
  `setting_reset` answers the same failure with `500`. `package_add` and
  `package_update` deliberately keep `400`: they are driven by user input
  (unknown AUR name, unreachable git remote, unparseable PKGBUILD) and report it
  as an untyped `anyhow` error we cannot tell apart from an internal fault.
  No success path changed status.
* **§15 — `vercmp`.** Now returns `Option<Ordering>`; `None` for an
  unparseable version instead of `Equal`, which callers read as "not newer" and
  which therefore froze such a package forever. The version-check loop gained
  `upstream_is_newer`, which on an uncomparable pair warns and falls back to
  "did the string change" — erring towards a wasted rebuild rather than a silent
  freeze.
* **§11 — `check_platforms` removed.** It asserted that a `Platform` is one of
  the `Platform` variants. `build_add_context` became infallible as a result.
* **§4 — `SnapshotStore`'s dead `client` parameter removed** from all six public
  methods and the two internals. `AurClient::new()` call sites went from 17 to 7
  (the survivors actually use the client).
* **§6 — `now_secs()` unified** into `aurcache_db::helpers::time`, collapsing six
  copies. `aurcache-worker` keeps its own: `aurcache-db` is only a
  *dev*-dependency there, and the binary should not gain it for a timestamp.
* **§7 — `aurcache-deps` errors are typed.** `Error` gained `Io`, `Url`, `Json`
  and `RepoDb` variants, so ~15 `.map_err(|e| Error::Rpc(e.to_string()))` sites
  became a plain `?` and the error source chain survives.
* **§16 — `CustomFileServer::from`** (an inherent `from` that just called `new`)
  removed.
* **§12 — the `static` feature is linted in CI.** `embed.rs` never compiled
  locally because `#[derive(RustEmbed)]` needs `aurcache-api/web`, which only the
  Docker build produces. CI now creates an empty placeholder (enough for the
  derive) and runs clippy with `--features static`. The main clippy invocation
  also gained `--all-targets`, so test code is linted too.
* **Empty build output is no longer a 404.** `GET /build/{id}/output` returned
  `404` for a build that exists but has not logged anything — the normal state of
  a build that just started, and the log view polls from the moment it opens. It
  now returns `200` with an empty body; `404` still means "no build with that
  id". The slicing moved into a pure `slice_output` helper with unit tests
  (empty, no offset, skip, skip-past-end, negative offset).

  This exposed a frontend bug the 404 had been masking: the poller sent
  `output.split("\n").length` as "lines I already have", and `"".split("\n")` is
  `[""]` — so with an empty log it asked the server to skip one line it had never
  received, dropping the first line of every build's output (and prepending a
  stray newline via `output += "\n$value"`). Both fixed in
  `lib/components/build_output.dart`.
* **Frontend.** `lib/api/builds.dart` returned `resp.statusCode == 400` from
  `deleteBuild`/`cancelBuild` — inverted, and unreachable anyway since Dio throws
  on non-2xx. Now `== 200`, matching every other API wrapper. (Both call sites
  discard the result, so nothing observable changes.)

### First round

**Reachable panics removed**

* `api/package.rs::get_package` and `utils/package/add.rs` both had
  `todo!("upload…")` on the `SourceData::Upload` arm. `AddPackage` deserializes
  `SourceData` straight from the request body, so a `{"type":"upload"}` payload
  panicked the handler. Now a `501` / an `Err`.
* `activitylog::deserialize_type` had `todo!()` for `StartBuild`/`FinishBuild`
  activity types; `list()` reads arbitrary DB rows, so any such row would panic
  the endpoint. Now an error that `list()` logs and skips.
* `pacman-mirrors::gen_mirrorlist` sliced `&mirrors[..10]` (see §1).
* `worker/config.rs::parse_size` did `&s[..s.len() - 1]` after reading the last
  `char` — a multi-byte suffix sliced mid-character. Also `n * mult` could
  overflow; now `checked_mul`.
* `api/custom_file_server.rs::parse_range_header` did `e + 1` on an attacker-
  supplied `u64`; now `checked_add`.
* `api/init.rs`: `Key::try_generate().unwrap()` → `.expect(…)`;
  `openapi.components.as_mut().unwrap()` → `let … else { return }`;
  `"0.0.0.0".parse().unwrap()` ×3 → `Ipv4Addr::UNSPECIFIED.into()`.
* `scheduler`: `.to_std().expect("Time went backwards?")` on cron deltas ×2 →
  `unwrap_or(Duration::ZERO)`.

**Wrong or misleading output**

* `settings/general.rs` logged `"Warning: Failed to fetch pkg-specific setting …"`
  on `Ok(None)` — i.e. on the completely normal "no per-package override" path,
  for every setting lookup. Now only a real query error warns. Also the three
  `eprintln!`s in that file became `tracing::warn!`.
* `aurcache-common/src/settings.rs` documented the precedence as
  `Env → Package → Global → Default`; the code (and `CLAUDE.md`) do
  `Package → Env → Global → Default`.
* `utils/package/update.rs::dependencies_ready_for_platform` had a doc comment
  that was cut off mid-sentence and merged with the next function's first line.
* `aurcache-cli::build_status_label` had no arm for state `4`, printing
  "unknown" for every build waiting on dependencies.
* `aurcache/src/startup.rs` logged the mirrorlist *directory* while writing the
  mirrorlist *file*.
* `worker/client.rs` had a doc comment about CA fingerprints attached to
  `base64_decode`; moved to `ca_fingerprint`.
* `worker/chroot.rs`'s base-chroot lock claimed to be "keyed by chroot dir" but
  is a single global lock.
* `api/aur.rs` `search` was documented as "Get all todos".

**Silently discarded errors**

* `aurcache/src/main.rs`: `let _ = post_startup_tasks(&db).await;`
* `scheduler/update_version_check.rs`: three `let _ = package_model.update(&db)`
  → a `save_package` helper that logs.
* `scheduler/mirror_ranking.rs::update_mirrorlist` warned about a failed status
  fetch and then returned `Ok(())`, so the caller's error branch never fired.
* `snapshot.rs`: `let enc = tar.into_inner()?; drop(enc);` swallowed gzip finish
  errors in both archive builders; now `.finish()?`.
* `activitylog`: `.map_err(|e| anyhow!(e.to_string()))` dropped the error source
  (×2); `update_version_check` did the same for the AUR RPC error.

**Dead code removed**

* `aurcache-utils/src/platforms.rs` — an empty orphan file not in `lib.rs`.
* `git/checkout.rs::checkout_repo_ref` and `checkout_git_source` — zero callers.
* `aurcache_deps::Error::NotFound` — never constructed.
* `builds::Relation::Package` — a `has_one` duplicate of `Packages`, unused.
* `job_config::create_makepkg_config` returned a hardcoded config path both
  callers discarded; it and `build_job_config` are now infallible.
* `worker/cache.rs`: an `#[allow(dead_code)]` that wasn't needed (the item is
  `pub` in a lib crate).

**Duplication factored out**

* `worker_store.rs`: `approve_worker`/`revoke_worker`/`store_signed_cert` shared
  a load-mutate-update body → `load_for_update` + `set_status`.
* `worker_complete.rs`: `complete_success`/`complete_failure` were ~35 identical
  lines each → `finish_build(db, build_id, worker_id, status)`.
* `pacman-repo-utils/repo_database/desc.rs`: `Display for Desc` cloned all 16
  `String` fields to feed `add_desc_entry(&self, …, String)`, which never used
  `self`, then `join("")`ed the results. Now two free `write_*` helpers writing
  straight into the formatter — no allocation, no clones.
* `aurcache-client`: three overlapping query-string helpers →`Query::opt`.
* `aurcache-cli`: ten copies of
  `match format { Json => print_json(&x), Text => { print_x(&x); Ok(()) } }` →
  a `render(format, &value, printer)` helper.
* `cli/config.rs::ensure_interactive` inlined `is_interactive`'s body.

**Signatures / ownership**

* `pacman-repo-utils` took `String` by value everywhere for read-only paths:
  `repo_add`, `repo_remove`, `add_to_db_file`, `remove_from_db_file`,
  `Pkginfo::set_signature`, `calc_checksums`, `parse_line`. Now `&Path`/`&str`,
  which also removed a pile of `.clone()`/`.to_string()` at the call sites and
  let `repo_ingest` stop building paths with `format!`.
* `init_repo(&PathBuf)` → `&Path`; `aurcache-ca::write_secret(&PathBuf)` →
  `&Path`; `add.rs::check_platforms(&Vec<Platform>)` → `&[Platform]`.
* `worker/job.rs`: `&Arc<AtomicBool>` / `&Arc<Mutex<…>>` parameters → `&AtomicBool`
  / `&Mutex<…>`.
* `utils/package/update.rs`: `services: &mut Services` in six functions was never
  used mutably.
* `worker/main.rs`: `--path` is a `PathBuf`, not a `String`.
* `scheduler/mirror_ranking.rs::start_mirror_rank_job` took a `DatabaseConnection`
  and a `Sender<Action>` it never used.
* `scheduler/auto_update.rs::start_auto_update_job` returned
  `anyhow::Result<JoinHandle>` but never failed; its handle is now joined in
  `main`'s `select!` like the other jobs.
* `aurcache_deps::official_dependency_exists` returned `Result<bool>` that was
  always `Ok` (it swallowed the error internally); now `-> bool`, with a doc
  comment saying why the swallow is intentional.
* `benchmark::gen_mirrorlist` was a `Bench` trait method taking `&self` *and* the
  mirrors — hence `urls.gen_mirrorlist(urls.0.clone())` at the call site. Now a
  free function.

**Idiom / readability**

* `is_in(vec![…])` → `is_in([…])` (5 sites); `Box::from` → `Box::new` (5 sites);
  `..std::default::Default::default()` → `..Default::default()`.
* `Err(DbErr::Migration(…))?` → `return Err(…)` across 20 migration sites.
* `db as &DatabaseConnection` → `db.inner()` (27 sites in `aurcache-api`).
* `impl Into<Vec<Route>> for CustomHandler` → `impl From<CustomHandler> for
  Vec<Route>`; `CustomHandler {}` → `CustomHandler`; `BuildStates {}` →
  `BuildStates`.
* `models/authenticated.rs` parsed a cookie value into a `String` and threw it
  away just to test presence.
* `vcs_check.rs`: a `matches!` pre-filter plus an `unreachable!()` arm collapsed
  into one `match`.
* `update.rs`: an empty `if` branch used as a comment → a `&&` condition.
* `repo_init.rs::repo_exists` returned `anyhow::Result<()>` used only via
  `.is_ok()`; `_ = fs::create_dir_all(path)` ignored its error.
* `db/init.rs`: `if fs::metadata("./db").is_err() { create_dir(…) }` →
  `create_dir_all`; `startup.rs` mixed `std::fs::metadata` into an otherwise
  `tokio::fs` function.
* `startup.rs::pre_startup_tasks` was `async` with no `.await`; so were
  `api/worker.rs::get_ca` and `get_ca_fingerprint`.
* `worker/cache.rs::plan_eviction`: manual index loop → `for` + `break`;
  `in_use.iter().any(|u| u == &x)` → `in_use.contains(&x)`.
* `worker_jobs.rs`: `use` statements inside a function body, a fully-qualified
  `std::collections::HashSet` return type, and a hardcoded `"approved"` literal
  where `STATUS_APPROVED` exists.
* `SnapshotStore::with_checkout_root` duplicated the other constructor's body.
* `Pkginfo::new()` listed all 19 fields by hand → `#[derive(Default)]`.
* `packages.rs::FromStr` had a pointless `let value: Self = …; Ok(value)`.
* File renamed: `aurcache-api/src/cusom_file_server.rs` →
  `custom_file_server.rs`.
* `api/build.rs::rery_build` → `retry_build`.
* `api/package.rs`: the 200-line `#[cfg(test)] mod tests` sat in the middle of
  the file, with `get_package` after it; moved to the end.
* `tests/add.rs`: a struct field named `_checkout_dir` that is actually read.
* Assorted: `stats.rs` matched on float literals; `startup.rs`/`repo_ingest.rs`
  built paths with `format!`; `worker/cache.rs::ensured` took an unused `&self`;
  `api/package.rs::normalize_build_flags` was a function that only mapped over
  its `Option` argument.
