# Rust idiom review — 2026-08-19, verified and corrected 2026-08-20

Full pass over `backend/` (~140 files, ~24k lines). The original review ran
`cargo clippy` at default + pedantic/nursery levels, applied small fixes
directly, and left larger items for a judgement call. That work was then
independently verified; this document records the corrected result.

**Guiding principles, confirmed with the repo owner:**

1. The goal is idiomatic and readable code, not pedantic-lint compliance.
2. Don't drop a lint workspace-wide to accommodate a single site. Enable it and
   add a local `#[expect(clippy::x, reason = "...")]` instead. A rule is left
   unadopted only when we object to it generally.

Both are encoded in `backend/Cargo.toml` under `[workspace.lints.clippy]`,
whose comment block carries the policy and the short list of lints not adopted,
so a future pass does not re-churn these decisions.

## Verification status

`cargo clippy --all-targets --workspace -- -D warnings` clean, `cargo fmt
--check` clean, `cargo test --workspace` passing except three
`aurcache-utils` `snapshot::tests` failures. Those three were checked against a
detached worktree at HEAD and fail identically there (`PKGBUILD parsing failed
and no fixes were applied` — the `alpm-pkgbuild-bridge` dev dependency), so they
are pre-existing and unrelated to this work.

**Caveat on all counts below:** the original review counted with
`--all-targets`, which compiles lib and test targets separately and reports each
warning twice. Every figure in the first draft was therefore inflated ~2x. The
numbers here are deduplicated.

## Applied and kept

- **`Self` instead of repeating the type name** in `impl` blocks
  (`clippy::use_self`) — constructors, `match self` arms, etc.
- **Eager error/closure construction** in `unwrap_or`/`ok_or`/`map` →
  `unwrap_or_else`/`ok_or_else`/`is_some_and`/`is_ok_and`.
- **`push_str(&format!(...))`** → `write!`/`writeln!` in `job_config.rs` and
  `pacman-mirrors/benchmark.rs`.
- **`match`-on-`ActiveValue` boilerplate** → `let...else` in
  `cusom_file_server.rs` and the dependency-resolution migration.
- Redundant `.clone()` before a move, `assigning_clones`, `needless_collect`,
  `uninlined_format_args`, a stray `continue` at the end of a loop body
  (verified: the `match` was the last statement in the loop, so removing it does
  not change control flow), `.collect::<Vec<_>>().join("")` →
  `.collect::<String>()`.
- **`use sea_orm::*;`** in `settings/general.rs` (the only wildcard import in
  the backend) → explicit imports.
- **Reachable panics removed.** Two of these are genuine bug fixes rather than
  style:
  - `aurcache-api/src/embed.rs`: the static-file handler unwrapped the parsed
    request path unconditionally, so a path failing Rocket's segment decoding
    panicked the handler. Now returns `400`.
  - `aur/api.rs` and `pacman-mirrors/benchmark.rs`: sorting externally-sourced
    `f32`/`f64` via `.partial_cmp(..).unwrap()` → `.total_cmp(..)`, removing a
    NaN panic.
  - Also: `stats.rs` `.to_u32().unwrap()` → `.unwrap_or(0)`; `main.rs`
    `.unwrap()` on `init_db()` → `.expect(..)`; `package/add.rs`
    `saved.id.clone().unwrap()` → the existing `ActiveValueExt::get()` helper.
- `aurcache-worker/src/cache.rs`: `scan_srcdest` dropped its `io::Result`
  wrapper. Beyond the scope of an idiom pass, but correct — the `Err` arm was
  unreachable, so the `tracing::debug!` it fed was dead code.

## Corrections applied to that pass (2026-08-20)

The pass introduced or left the following; all are now fixed.

1. **A new lint was introduced.** `worker_enroll.rs` became
   `.ok().is_some_and(..)` on a `Result`, tripping `manual_is_variant_and`.
   Now `.is_ok_and(..)`.
2. **`desc.rs` got worse, not better.** The unwrap removal was right, but the
   replacement was two `matches!` calls (`matches!(values, []) ||
   matches!(values, [v] if v.is_empty())`) where the original was one condition.
   Now a single `match` on the slice.
3. **Six `pub(crate)` → `pub` widenings in `aurcache-deps` reverted.** The
   justification was technically sound — `lib.rs` declares those modules
   privately, so nothing leaked — but `redundant_pub_crate` is a contentious
   nursery lint that contradicts rustc's own `unreachable_pub`, and the change
   deleted intent for no gain.
4. **Eleven `map_or_else`/`map_or` inversions reverted** in `snapshot.rs`,
   `config.rs`, `repo.rs` and `cli/main.rs`. `map_or_else(|| default, f)` places
   the fallback before the transform, inverting reading order.
   `.map(f).unwrap_or_else(|| default)` reads correctly. `map_or` with a
   constant default (the `now_secs()` helpers, `logger.rs`) was kept, as was
   `detect_nproc`'s `.map(..).unwrap_or(1)` shape.
5. **`or_fun_call` was listed as applied but was ~15% done** — 4 sites fixed, 21
   left, so the codebase did it both ways. Now complete across `api/build.rs`,
   `api/package.rs`, `db/init.rs`, `utils/package/{delete,live_check}.rs`,
   `scheduler/mirror_ranking.rs` and `pacman-repo-utils/repo_add.rs`.

## Structural work done

- **`aurcache-api/src/stats.rs::get_stats`** (was 111 lines) split into
  `avg_build_time`, `build_trends` (+ `BuildTrends` and a `build_trends_query`
  helper holding the dialect-specific SQL), leaving `get_stats` at 28 lines.
  Both raw SQL literals are byte-for-byte unchanged.

  Note while you are in there: `avg_build_time` passes `DbBackend::Sqlite` to
  `Statement::from_sql_and_values` unconditionally, including on Postgres. It
  works today because the statement takes no bind parameters and the SQL is
  dialect-neutral, but it is inconsistent with `database_type()` used a few
  lines below. Left as-is — changing it is a behaviour change, not an idiom fix.

## Corrections to the original findings

**Proposed item #1 was wrong and is withdrawn.** It claimed `too_many_lines`
flagged five functions in `aurcache-utils/src/package/update.rs` at 200/193/150/
133/102 lines, called this "the single strongest structural signal in the
review", and proposed splitting them into named helpers such as "sync dependency
rows" and "decide rebuild fan-out".

Those five functions are at lines 915, 1064, 1284, 1496 and 1753. `mod tests`
starts at line **719**. All five are `#[cfg(test)]` test functions.

The production half of `update.rs` is already decomposed into 17 named
functions, the longest being `package_update_with_client_inner` at 87 lines —
under the threshold. The helpers the item proposed creating already exist:
`sync_dependency_rows` (line 458) and `enqueue_platform_builds` (line 623). The
measurement used `--all-targets` and never checked which side of line 719 the
hits fell on. No action needed.

**Item #3 was real but overstated in difficulty.** `auth.rs::oauth_login` did
unwrap `get_redirect`, inconsistent with `oauth_callback` next to it. The review
called changing the return type "more than a one-line change to do safely";
in fact there is one mount site (`init.rs:151`) and one `#[openapi(paths(..))]`
entry, and the sibling handler already returns a `Result` through the same
registration. Now returns `Result<Redirect, Status>`, logging the cause.

**Two lints were initially declined and both declines were wrong.**

`unnecessary_wraps` on `cli/main.rs::print_done_message` was declined on the
grounds that CLI handlers return `Result` for uniformity. That was wrong on the
facts: it is not a command handler but a private print helper, it is
infallible, and the convention among its actual siblings is the opposite —
`print_worker_list` and `print_package_summary` return `()`, while
only `print_json` returns `Result`, because serialization can genuinely fail.
It now returns `()`, with `Ok(())` at the six call sites, and the lint is
enforced.

`redundant_closure_for_method_calls` was declined over its single site,
`worker/config.rs::detect_nproc`. Turning off a sound rule workspace-wide for
one case is the wrong trade — and on a second look the case was not worth
making at all: `.map(std::num::NonZero::get)` and `.map(|n| n.get())` read
about equally well. The lint is enforced and the site simply conforms, so the
tree carries no suppression for it.

**Item #4's count was 274; the real figure is 137** unique `missing_errors_doc`
sites. Declined either way — see the lint table.

## Open / not doing

- `too_many_lines` on production code, remaining and genuine:
  `scheduler/update_version_check.rs::check_versions` (114),
  `utils/repo_ingest.rs` (137), `utils/package/add.rs` (115). Worth splitting if
  you touch them; not urgent. The 153-line dependency-resolution migration is a
  one-shot data migration and is reasonably exempt.
- `future_not_send` (4 sites in `aurcache-client`) — not investigated.
- `significant_drop_tightening` (2 sites in `utils/build_logger.rs`) — not
  investigated.
- The `snapshot::tests` failures are a real, separate problem worth its own
  look; they are an environment/dev-dependency issue, not an idiom one.
