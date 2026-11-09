# Transitive dependency views and the dependency graph

The package page shows only direct edges: what a package declares, and what
declares it. It cannot answer the two questions anyone touching a package
actually asks — "what does this pull in" and "what rebuilds if I change this".
This doc proposes three views over the same data that answer them: transitive
lists, a per-package dependency graph, and a repo-wide graph for debugging.

Status: **Proposed** · Last updated: 2026-10-05

---

## Current state

Edges live in `dependencies` (`dependent_id`, `dependee_id`,
`version_constraint`; `backend/aurcache-db/src/dependencies.rs`), with foreign
keys cascading on both package columns. The package-detail route fans out one
join query per direction (`list_package_relations`,
`backend/aurcache-api/src/package.rs:785`, called concurrently from
`get_package` at `:865`) and returns them as `dependencies`/`dependents` on
`ExtendedPackage`. The frontend renders those in two `RelationList` cards
(`frontend-rs/src/screens/package.rs:498`): direct edges only, editable on the
dependencies side.

Three precedents shape what follows:

- Raw per-backend SQL exists (`stats.rs:104` builds one query string per
  `DbBackend`), but SeaORM 2.0 also executes built statements directly
  (`db.query_all(&select)`), which nothing in the tree uses yet.
- Charts are native SVG: `BuildsChart` (`dashboard.rs:749`) renders through
  `dioxus-charts`. The graphs below go one step further — SVG rendered by our
  own components, so they can be interactive.
- The frontend has almost no JS interop (a `js_sys::Date` call, a clipboard
  promise) and no npm asset pipeline — the build runs wasm-bindgen for the
  `web` target (`bindgen` in `aurcache-api/build.rs`). Anything needing a JS library is
  a new kind of dependency for this codebase.

## Phase 1: transitive lists

One recursive CTE per direction, executed only when asked — built with the
typed query builder, not raw SQL. Entity-level `Entity::find()` cannot express
the self-reference, but the sea-query `SelectStatement` one layer down (what
entities compile to) can: `WithClause::recursive` with a single
`CommonTableExpression` over a UNION of the base leg and the recursive leg,
with entity `Column` enums plugging in as identifiers. One builder renders
`WITH RECURSIVE` for both the SQLite and Postgres backends; SeaORM 2.0
executes it portably via `db.query_all(&select)`, and rows map through
`FromQueryResult` into the existing row struct. This would be the first
typed-statement execution in the tree — everything today goes through the
`*_raw` methods — but it is the blessed 2.0 API.

- **API.** `GET` package-detail gains a `?transitive` parameter;
  `ExtendedPackage` gains `transitive_dependencies` / `transitive_dependents`,
  populated only when asked. `PackageDependency` gains `depth` (shortest-path
  length, 1 = direct) and `via` (the immediate parent name on that path). The
  direct fields never change shape or cost, so the CLI, docs, and the
  builder-promotion parity rule are untouched. All shapes live in
  `aurcache-common`, never re-declared.
- **Row semantics stay leaf-local.** `satisfied`/`built_version` keep the exact
  direct rule — the leaf's newest successful build against the edge constraint
  into it. No path-conjunctive "is the whole chain satisfied": expensive to
  compute, and a single red ancestor would paint every row red without saying
  which one is broken.
- **Cycle safety is structural and portable.** Repointing can introduce mutual
  and self edges, so the CTE uses `UNION` (dedup), a depth cap (~25), and a
  visited guard. Deliberately not the `CYCLE ... SET ... USING` clause from
  sea-query's docs example: that is Postgres-only and would break SQLite
  parity. Termination is covered by a dedicated cycle test, not by forbidding
  cycles.
- **UI.** A Direct|Transitive toggle in each card header, defaulting to Direct
  so no page pays for a closure it does not show; transitive rows carry a depth
  badge and link to their package pages as today.
- **Speed.** One round trip per list; no per-level queries. Verify covering
  indexes on `dependencies(dependent_id)` / `dependencies(dependee_id)` (the FK
  migration should have created them) and add a migration if either is missing.
  Assumption: graphs here are hundreds of edges — verified during
  implementation with a timing test, not a perf harness.

Rejected: app-side BFS (N+1 per level, or a full edge-table load per page
view); a closure table maintained on write (fastest reads, but write-path sync
machinery for a read-rarely view on a small graph).

## Phase 2: the per-package graph

Layout in pure Rust, rendering in native Dioxus SVG — interactive from the
start, with no system dependency and no JS. `rust-sugiyama` (a cargo
dependency, MIT, `petgraph` + `log` only) implements the real Sugiyama layered
layout and returns node coordinates per connected component; our components
draw the nodes and edges as SVG. Clicks and hover work natively, pan/zoom is
viewBox state in pure Rust event handlers, and theming falls out of the
existing CSS.

- **Endpoint.** A graph route per package and direction serving a JSON layout:
  nodes with positions, sizes, build status, and `directly_requested`, plus
  the edge list. The Dioxus side renders it; the CLI and scripts consume the
  same JSON. One endpoint with a direction parameter covers dependencies and
  dependents.
- **Data.** The graph needs the edge list, not just reachable nodes, so the
  Phase 1 CTE is written to return `(parent, child, depth)` and both the list
  UI and the graph consume it.
- **Placement.** A button in each `RelationList` card header opens the graph in
  that direction in a wide dialog, following the dialog idiom this page
  already uses (backdrop, click-away and Escape to close), with a direction
  switcher inside. The layout is fetched lazily on open, so a normal page view
  pays nothing. A package sub-route stays the escape hatch if the graph feels
  cramped — the codebase notes in two places that a modal has less room than a
  page.
- **Node encoding, shared by every graph.** Two node dimensions need two
  channels: fill is the build status, reusing the badge palette so the graph
  reads the same as the lists; the border carries direct-vs-dependency-only
  (solid and heavier for directly requested, dashed or lighter for
  dependency-only). The current package gets a focus ring in per-package
  views. A legend explains both channels and hover tooltips carry the status
  text, so color is never the only carrier — `directly_requested` has no
  visual precedent elsewhere, it is list-filter-only today, so the legend does
  real work.
- **Where the layout runs.** Server-side first: positions are computed on
  request, cacheable per package, and keep the wasm bundle lean. The noted
  variant is running `rust-sugiyama` in wasm on the client — it is pure Rust
  and should compile to `wasm32` cleanly — with the server shipping only the
  edge list. Either side is trivial at our graph sizes; the variant needs a
  wasm target check before it can be chosen.
- **Edges and boxes are ours.** The crate returns node positions only, no edge
  bend points, so edges are straight or orthogonal lines — below `dot`'s
  splines, fine at our sizes. Node boxes come from label-width estimates
  passed in as vertex sizes.
- **Labels are untrusted input.** Package names come from the AUR and git
  remotes; rendering them through `rsx` text nodes makes escaping automatic,
  but a test with quotes, angle brackets, and a newline is part of the
  deliverable anyway.
- **Large-graph guard.** A node cap (order of a hundred) with a "showing N of
  M" note, so a pathological closure degrades into a message rather than an
  unreadable poster.

Rejected: the Graphviz binary (a system dependency plus hardcoded SVG colors,
against a cargo dependency plus full rendering control); JS graph libraries
(Cytoscape, ECharts, sigma) — the first real JS dependency, needing a vendored
asset story through the wasm-bindgen build plus interop glue, justified only
if force-directed physics or drag-to-rearrange is ever wanted; hand-rolled
layered layout — `rust-sugiyama` already is that work, done against the
literature.

## Phase 3: the repo-wide graph

The same renderer over the whole repository, for debugging: cycles, orphaned
dependency-only packages, "why is X even here" reverse reachability, and
repointing mistakes are all visible at a glance in a way no per-package view
shows. `rust-sugiyama` divides the graph into connected components itself, so
isolated clusters pop out for free.

- **Data is the easy half.** No CTE: one query dumping the whole edge table
  plus per-package names, statuses, and `directly_requested`.
- **Placement.** A dedicated route (e.g. `/packages/graph`), linked from the
  Packages list header beside the existing filters. Full page width and
  deep-linkable; a dialog is the wrong size for repo-wide.
- **Focus makes it usable.** Clicking a node highlights its transitive
  dependencies and dependents and dims the rest — without that the view is
  decoration. Plus node search, the list's direct-vs-dependency filter
  semantics, the Phase 2 node encoding and legend, and the same node cap.
- **Freshness without jumping.** The layout must not re-run on a poll timer —
  the packages list polls briskly while building, but a re-layout on every
  tick would make the graph jump. Refresh on demand with a stale indicator,
  and cache the server-side layout invalidated on dependency changes (or
  compute on demand if the timing test says it is fast enough).

## Validation

- New `aurcache-api` tests: chain (a→b→c), diamond (dedup), 2-cycle plus
  self-edge (termination), both directions, depth values; graph tests assert
  the layout JSON shape, sane positions, and escaping of nasty labels; the
  repo-wide route's filter/focus helpers tested as pure functions beside the
  listing helpers.
- `cargo test -p aurcache-api`, then `just test`, `just lint`, `just format`
  (lint already covers the frontend for wasm as well as host).
- `./scripts/test-frontend.sh` — it asserts every route mounts, so the new
  repo-wide route is covered by adding its URL to the route list; the package
  route still boots with the toggle and graph dialog present.
- The new route round-trips through the `Route` URL test.
- `EXPLAIN QUERY PLAN` on SQLite shows index use for both CTE legs.
- SQLite/Postgres CTE parity stays on the checklist — one builder renders for
  both, but Postgres is still verified via the compose dev setup before merge.

## Risks and open questions

- `rust-sugiyama` is quiet (no release since mid-2024) though actively used;
  a layout algorithm going quiet usually means done, and our graphs are small
  enough that mis-layout would show immediately rather than lurk.
- The repo-wide layout is the most expensive render here; if the timing test
  rules out computing it on demand, the cache needs an invalidation story tied
  to dependency writes.
- The depth-cap value, the node caps, the `via` field name, the server-vs-wasm
  layout side, and the cache strategy are the review-level choices; all have
  reversible defaults in this doc.
- The "hundreds of edges" scale assumption, if wrong, flips Phase 1 toward
  pagination or a closure table — the timing test settles it.
