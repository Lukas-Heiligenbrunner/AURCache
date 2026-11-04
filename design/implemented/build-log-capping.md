# Design: Capping the build-log view

Status: **Implemented** · Last updated: 2026-09-22

On mobile Chrome the build page for a log that decompresses to ~40 MB dies with
"Aw, Snap!" — the renderer process is killed. The transfer isn't the problem
(per-poll chunks are ~1.3 MB compressed); the problem is that the UI keeps
every byte of the log as one `String`, rendered as a single `<pre>` text node,
and grows it without bound. This design caps the DOM content structurally — a
rolling window of the newest ~4 MiB (per device, see below) — so that no build,
however large and however long it is watched from line one, can ever make the
renderer hold the whole log.

Two invariants drive everything that follows:

- **The endpoint is always bounded.** "Unlimited" is never an option: the server
  never does a whole-log read, on any path.
- **Every client can paginate.** Clients that want the whole log page through
  it; the rendered window is a UI choice, not a transfer cap.

## Motivation

`frontend-rs/src/screens/build.rs` polls `GET /package/<pkgbase>/build/<number>/
output?offset=<bytes>` every 3 s and appends each chunk to an unbounded
`log: Signal<String>` (`build.rs:209`) rendered as one text node
(`build.rs:449-450`). Three things scale with the *whole* log, not with a poll:

- **Memory.** The Rust `String`, its UTF-16 JS-string copy (2 bytes per char),
  and the DOM text node; a 40 MB ASCII log is ~80 MB of JS string plus shaping
  bookkeeping before the crash.
- **Re-layout.** The node is `whitespace-pre-wrap`, so every poll re-shapes the
  entire node — a growing, multi-second freeze.
- **The first request.** `offset=0` makes `read_build_output`
  (`build_logger.rs:57-85`) `read_to_end` a multi-GB file into server RAM, and
  ships all of it to a client that only asked to see the page.

The cap is a *UI* invariant, not a transfer cap: the newest few MiB is always
enough to read a build's tail and recent error context, and
`aurcache-cli builds output` (with `--offset`) remains the tool for the full
log. One `<pre>` text node stays deliberate — per-line elements are what the
comment at `build.rs:204-208` already ruled out — we only stop making it grow.

## Design

### The endpoint contract: always bounded

`GET /package/<pkgbase>/build/<number>/output?offset=<u64>&limit=<u64>`

- `offset` — optional, default 0. The server seeks to it in the log file.
- `limit` — optional, **default `MAX_OUTPUT_BYTES` (32 MiB)**, and any provided
  limit is clamped to it. This is structural: `read_build_output` takes a
  bounded read on every call, so neither an absent `limit` nor a caller-configured
  one can make the server `read_to_end` a multi-GB file (review E).
- The response body is **raw bytes** — exactly the slice the requested window
  covers, with no server-side UTF-8 munging. The old leading-continuation skip
  (`build_logger.rs:77-82`) disappears; the *client* owns alignment and position
  arithmetic, so the returned length is always the true byte length. An empty
  body is 200 and means "nothing new yet" — the ordinary state of an idle poll.

There is one `/output` endpoint and one code path; a request is a bounded window
read with optional positioning, never a whole-log read. An HTTP `Range` header
would model the same shape, but **query params win**: they leave the addressing
model free (byte offsets today; line numbers or a cursor later), whereas `Range`
bakes byte-addressing into the transport. `Range` remains available where it is
already free — `NamedFile` serves `/output/download` and handles it natively.

### Client-owned alignment and position

The server returns raw bytes, so each client turns a page into text with a pure
`align` helper and derives its next position from the slice it actually holds:

- **`align(raw: &[u8]) -> (front_skip, back_drop)`** — drop the *leading
  continuation run* (its lead byte is outside the window, so the run is dead
  bytes) and the *trailing incomplete codepoint* (an unfinished character cannot
  be displayed). `raw[front_skip .. raw.len() - back_drop]` is valid UTF-8 by
  construction. A slice that is all continuation bytes yields an empty display.
- **`next_offset = requested_offset + raw.len() - back_drop`** — always. The
  `back_drop` bytes are the trailing incomplete codepoint that `align` stripped;
  by rewinding `next_offset` past them, the next page starts at the beginning of
  that codepoint so it reappears whole rather than being dropped. Clients never
  reuse an old position or merge windows, so there is no duplication and no spin:
  a page ending mid-character re-reads the straddling codepoint on the next
  fetch, and at EOF the next page is empty and the pager stops. At most 3 bytes
  per window boundary are re-read (a 4-byte codepoint starts 1 byte before the
  boundary), and ASCII logs never hit it.

### The window

`fn append_capped(window: &mut String, chunk: &str, cap: usize)` — in place
(`String::drain`, no fresh multi-MiB allocation per poll; `usize`, review 5).

- While the accumulated string is ≤ cap it is passed through untouched — a
  small log renders exactly as today, from line one.
- Once it would exceed cap, **trim from the front** to the trailing cap budget.
  The cut lands on the `\n`+1 boundary of the surplus so the window starts at a
  line start — which is also a UTF-8 character boundary for free, since a
  U+000A byte can never appear inside a multi-byte sequence. The drained buffer
  keeps its allocation, so later appends land in place.
- **Oversized single line:** if the whole window is one line (a pathological
  build), byte-trim to cap and `drain` any leading continuation bytes rather
  than dropping the window.

Effective occupancy ≤ cap + one line; the cap is approximate by design.

### Per-device caps

`fn log_tail_cap(width: Option<f64>, coarse_pointer: bool) -> usize` (pure,
host-tested):

- `None` (no window — headless, embedding) → `MOBILE_LOG_TAIL_BYTES` (4 MiB)
  as the mobile-safe default,
- `width < 768` → `MOBILE_LOG_TAIL_BYTES = 4 << 20`,
- `coarse_pointer` → `MOBILE_LOG_TAIL_BYTES` — the primary pointer is touch
  (`matchMedia("(pointer: coarse)")`): phone- or tablet-class hardware with a
  mobile renderer budget, whatever the viewport width,
- else → `DESKTOP_LOG_TAIL_BYTES = 16 << 20`.

The two signals fix width-only detection's blind spot: the crash this design
guards against is a renderer *memory budget*, and width is a poor proxy for it
on a foldable phone. Unfolded and zoomed out, such a device reports a
tablet-wide viewport while still running a phone-class renderer that can only
grow ~4 MiB of DOM safely — `coarse_pointer` catches it where `width` cannot.
The pointer check is the primary pointer only, so a desktop with a touchscreen
keeps the desktop cap (its trackpad/mouse is the primary device; only
`any-pointer: coarse` would be true). Both signals come from
`web_sys::window()` — `inner_width()` and
`match_media("(pointer: coarse)").and_then(|m| m.matches())` — the same
`web_sys::window().and_then(...)` pattern the codebase already uses, and
`prefers_dark`'s `match_media` call is the exact precedent (`theme.rs:160`,
`theme.rs:166-170`, `poll.rs:61`, `dates.rs:275`, `api.rs:10`). Read once when
the Build screen mounts; Dioxus 0.7 has no `use_window_size`, and a live
`resize` listener adds nothing — the crash guard just needs *a* bound, and a
phone rotated wide still lands on the mobile cap via its pointer. The device
cap is three things at once: the per-poll `limit` sent to `/output`, the
`append_capped` budget, and the "shown tail" label. The CLI knows no cap — it
paginates.

### Page load and the polling loop

Per poll, **`get_build` runs first** — status, `worker_name`, times, and the
new `log_size` — and the output fetch is gated on it:

- **`get_build` fails** → show the error banner and skip the output fetch this
  cycle (review D). The first output fetch therefore only ever happens after a
  successful `get_build`, so a transient failure can never fall back to
  `offset = 0` on a huge log.
- **First output fetch, once `get_build` succeeds:** `log_size` is `None` (no
  log file yet) or ≤ cap → `offset = 0`, today's behaviour. `log_size > cap` →
  `offset = log_size − cap`; the handler tolerates an arbitrary mid-character
  offset, and seeking to `size − cap` can only over-run by bytes appended in the
  milliseconds between the size read and the seek. The frontend then strips the
  **leading partial line** on a tail placement (review H): after `align`, drop
  everything through the first `\n`, so an initial view starts at a line start
  instead of mid-line.
- **While following:** fetch one page (`offset = last_next`, `limit = cap`),
  `align`, `append_capped`, advance `next_offset = requested + raw.len() - back_drop`.
- **While *not* following (review G):** skip the output fetch entirely, so
  nothing is appended and nothing is trimmed while the user reads back through
  the log — the view cannot jump under the scroll offset every poll. (The
  header's `get_build` polling continues so status stays live.) Output resumes
  from the untouched `next_offset`, so no bytes are lost or re-fetched. When the
  build **settles**, one final sync happens regardless of the follow switch —
  the view gets its last page once and then never moves again, which is a single
  trim rather than the repeated jumping review G describes.
- Poll stops once the build reaches a terminal state, exactly as today
  (`settled`, `build.rs:26-28`).

### Copy and Download both stay

- **Copy** (`LogCopyButton`, `build.rs:492-539`) keeps its behaviour. Once the
  window is a true tail (trimmed at least once, or placed at `offset > 0`), it
  gains a **warning marker** (a small `!` glyph added to `shell.rs`, in the
  `ButtonIcon` family) and a native `title` tooltip: "Copying the shown ~4 MiB
  tail, not the full log — use Download for the whole log", with the label
  flipping to **"Copy tail"**. Copying is always ≤ cap + one line either way, so
  the clipboard path stays safe (review F).
- **Download log** is a plain `<a href="/api/package/<pkgbase>/build/<number>/
  output/download" download="<pkgbase>-<number>.log">` — the page is
  session-cookie authenticated, not Bearer (`frontend-rs/src/api.rs:20-23`;
  `authenticated.rs:29-38` reads the private `token` cookie), so a bare anchor
  carries the session and the browser streams the response to its download
  manager with zero JS-heap involvement (review B). The earlier
  `fetch -> Blob` plan is rejected: `response.blob()` buffers the whole body in
  renderer memory and would re-trigger the crash.

### Footer

`build.rs:453`. Untrimmed: `"{line_count} lines"` as today. Trimmed: uses the
`format_bytes` helper (`format.rs:35`) for the total from `log_size` and counts
lines in the window: **"showing the last ~N MiB of {total}
(~{window.lines().count()} lines in view)"** (review I).

## Changes

### Server

**1. `build_logger.rs` — bounded raw reads (`backend/aurcache-utils`)**

`read_build_output(pkgbase, number, offset, limit) -> anyhow::Result<Option<Vec<u8>>>`
— `Option` still means "no log file"; the result is now **raw bytes** (no
`from_utf8_lossy`, no continuation skip). Seek to `offset`, then
`file.take(resolved_limit)` where `resolved_limit` is the caller's `limit`
clamped to `MAX_OUTPUT_BYTES`, or `MAX_OUTPUT_BYTES` itself when absent.

**2. `build.rs` — the `/output` handler (`backend/aurcache-api`)**

Pass `offset`/`limit` through (defaults as above); return the raw bytes with a
plain-text content type. Fix the stale doc prose at `build.rs:39-49`
("`startline`" is long since byte offsets) and the poll comment at
`frontend-rs/src/screens/build.rs:194-197`, which still says "`?startline=N`".
Also update the client docs (`aurcache-client/src/lib.rs:455-459`) that promise
offsets "always land on a character boundary" — that is now the client's job.

**3. `builds.rs` — `BuildSummary.log_size` (`backend/aurcache-common/src/api/builds.rs`)**

`pub log_size: Option<i64>` — `None` when the build has no log file (the
"not-known is `None`" convention), not "zero bytes". `#[cfg_attr(feature = "db",
sea_orm(skip))]` and `#[serde(skip_serializing_if = "Option::is_none")]`,
mirroring `waiting_reason` (`builds.rs:49-51`).

**4. `build.rs` — fill it only on the detail route (`backend/aurcache-api`)**

`get_build` (`build.rs:279`) sets it from `build_log_size` (`build_logger.rs:88-93`,
a metadata `stat`, not a read) after `annotate_waiting`. **List endpoints never
fill it** (review C): `annotate_waiting`, shared by `list_builds` and
`list_package_builds` (`build.rs:146`), leaves `log_size = None`, so a 50-build
page costs zero extra filesystem stats — none of the list screens display the
field, and the CLI doesn't either. `into_summary`
stays synchronous; only `get_build` overrides the default `None` afterwards.

**5. `build.rs` — streaming download route (`backend/aurcache-api`)**

`GET /package/<pkgbase>/build/<number>/output/download`: same `build_by_number`
404 and `Authenticated` guard as the output route; 404 (not 200-empty) when the
log file is missing — the download route is a resource fetch, not a poll.
Serve the file with a streaming `NamedFile` (which already handles HTTP `Range`
and streams from disk; the repo's `custom_file_server.rs:2` is the precedent)
plus `Content-Disposition: attachment; filename="<pkgbase>-<number>.log"`.
This is the only way to walk a whole multi-GB log without re-materialising it
client- or server-side. Register the route in `backend.rs` inside
`routes![...]` and add it to `BuildApi` for OpenAPI.

### Client library (`backend/aurcache-client/src/lib.rs`)

Add `build_output_page(pkgbase, number, offset: Option<u64>, limit: Option<u64>)
-> Result<Vec<u8>>` — a bytes variant of `request_text` (`lib.rs:690`) that
passes `offset` and `limit` and returns the raw body. Every consumer
(frontend, CLI) gets exactly the bytes the server read, so the position
arithmetic is shared truth. Keep or fold `build_output` as a String convenience
caller as needed.

### CLI (`backend/aurcache-cli/src/main.rs`)

`builds output` (`render_build_output`, `main.rs:1871`; args `main.rs:621-632`)
**paginates internally**: from `--offset`, request bounded pages (`limit`
absent → 32 MiB pages server-side), advance by `raw.len() - back_drop`, stop on
an empty page — EOF always terminates because `next_offset` can only move
forward. In text mode, stream each page to stdout as it arrives (memory stays
page-sized); the straddling codepoint naturally prints whole because
`next_offset` rewinds to re-read it. In JSON mode, accumulate and emit the
single `{"output": ...}` shape as today. Optional `--limit N` caps the total
bytes printed. This keeps the CLI's "the tool for the full log" role while the
server provably never holds more than one page.

### Frontend (`frontend-rs/src/screens/build.rs`)

**6. New pure module (host-tested, like `listing.rs`)** — `log_tail_cap(width)`,
`align(raw)`, and `append_capped(window: &mut String, chunk: &str, cap: usize)`
as above.

**7. Poll loop rewrite** — `get_build` first each cycle (status, `worker_name`,
times, `log_size`); skip output on failure; initial placement from `log_size`
(gate on `get_build` success, strip leading partial line on tail paths); freeze
output while `!following()` with one final sync on settle; bounded page fetches
(`limit = cap`); `next_offset` via `align`'d raw length minus trailing
incomplete codepoint. `byte_offset` (the comment at `build.rs:210-216`) becomes
`next_offset` and is initialized to `requested_offset + raw.len() - back_drop`,
never "0 + chunk.len()" (review A).

**8. Widgets** — `LogCopyButton` gains a "window is a tail" prop (warning marker,
tooltip, "Copy tail" label); a Download anchor appears beside it when the log is
non-empty; the warning glyph is added to `shell.rs` in the `ButtonIcon` family;
footer switches to the trimmed readout above.

### Constants

- `MOBILE_LOG_TAIL_BYTES: usize = 4 << 20`
- `DESKTOP_LOG_TAIL_BYTES: usize = 16 << 20`
- `WIDTH_BREAKPOINT_PX = 768`
- Mobile-cap test: `width < WIDTH_BREAKPOINT_PX || (pointer: coarse)`
- `MAX_OUTPUT_BYTES: u64 = 32 << 20` (server clamp and absent-`limit` default)
- Frontend caps must stay < `MAX_OUTPUT_BYTES`; they do (4/16 vs 32 MiB).

## Edge cases

- **Log grows between size read and tail fetch.** The fetch covers ≤ cap + Δ
  (reads are bounded by the page limit); the window trims Δ, and `align` drops
  the trailing split codepoint from display — but `next_offset` rewinds past it
  so it reappears on the next page. Next poll re-reads the size.
- **`get_build` fails for a while (network blip, or no file yet).** `log_size`
  is `None`; the output fetch is skipped (not sent at `offset = 0`), so the DOM
  cannot exceed the cap. Placement and footer total self-correct once it
  returns.
- **`log_size = None` on lists.** The footer and placement only ever read it
  from the detail route, so lists are unaffected.
- **A single line longer than the cap.** Kept (byte-trimmed to cap, no data
  loss) rather than dropped; this is where the "cap + one line" slack is spent.
- **Mid-char window start.** `align` drops leading continuation bytes; the
  one-line fallback reuses the same rule.
- **Page boundary splits a character.** `next_offset` rewinds past the trailing
  incomplete codepoint, so it reappears whole on the next fetch — no data lost;
  the CLI prints it whole via the same mechanism.
- **A finished build whose log is over cap, opened fresh.** First frame is the
  tail via `offset = log_size − cap`; the server read is bounded; the leading
  partial line is stripped once.
- **Poller running at EOF.** Empty 200 body means "nothing yet", same as today;
  `next_offset` is unchanged, so the next poll re-reads the same position.

## Consequences

- The 40 MB case renders a flat ~4 MiB window on a phone (16 MiB on desktop);
  memory, re-shape cost and first-paint transfer are all constant for logs above
  the cap rather than proportional to the whole log. A crash at the cap would
  point at something *else* on the page — the fix removes the log-size
  dependency from this screen — and the caps are a one-constant dial.
- The whole log stays reachable two ways that don't involve the browser:
  `aurcache-cli builds output` (paginating, with `--offset`/`--limit`) and the
  streaming download route.
- The server never does a whole-log `read_to_end` on *any* path; a visitor can
  no longer make it slurp a multi-GB file for one screen, and the CLI's full
  dump streams page by page.
- Client ownership of alignment and position (no header, no server skip) keeps
  one code path and makes the returned byte length exact.
- Deliberately out of scope: real streaming transport (SSE/WebSocket). Polling
  already delivers incrementally; the defect was unbounded aggregation, not the
  transport.

## Verification

- Unit (`log_tail_cap`, host): narrow widths `0`/`400`/`767` → 4 MiB at any
  pointer; coarse pointer at any width (`768`/`1280`) → 4 MiB; fine pointer at
  `768`/`1280` → 16 MiB; `None` width → 4 MiB regardless of pointer.
- Unit (`align`, host): leading continuation run, trailing incomplete codepoint
  (all three truncation shapes), an all-continuation slice → empty display, and
  empty input.
- Unit (`append_capped`, host): grows unchanged under cap; past cap trims to a
  line start in place; the one-line fallback byte-trims and drains leading
  continuation bytes; a cap-sized chunk does not shave lines.
- Unit (build_logger): paged read returns exactly `limit` bytes; absent limit
  returns `MAX_OUTPUT_BYTES` (and never more); `offset` at/after EOF returns an
  empty page; `None` for a missing log. Existing byte-offset tests stay; the old
  `an_offset_inside_a_character_does_not_yield_invalid_utf8` becomes a
  byte-exact assertion (the server no longer decodes).
- Client/CLI: a page ending mid-character advances `raw.len() - back_drop`
  (re-reads the straddling codepoint on the next page); the pager terminates at
  EOF (empty page); `--offset`/`--limit` respected; character-split printing in
  text mode.
- Server: `log_size` is `None` for a log-less build and the true size for one;
  `None` on list endpoints and filled on `get_build`; the download route streams
  with the attachment filename, a 404 for an unknown build, and a 404 (not
  200-empty) for a missing log.
- Frontend harness: no download/Copy search on a missing log; Copy offered
  untrimmed; warning marker + "Copy tail" tooltip appear exactly when trimmed;
  Download anchor present once the log exists; footer switches shapes.
- `cargo test` both workspaces, `scripts/test-frontend.sh` (route assertions),
  then the 40 MB unreal build's page on a phone: flat tail, no "Aw, Snap".
- Full gate: `just lint`, `just test`, `just test-browser`.

## Review dispositions

- **A — `byte_offset` absolute position**: `next_offset = requested + raw.len() - back_drop`,
  rewinding past the trailing incomplete codepoint so it reappears on the next
  page (§ Client-owned alignment and position; Change 7).
- **B — `Blob`/auth**: download is a plain same-origin `<a download>` riding the
  session cookie; `fetch -> Blob` rejected (§ Copy and Download both stay;
  Change 5).
- **C — list I/O amplification**: `log_size` filled only by `get_build`; lists
  leave it `None` (Change 4).
- **D — unbounded fetch on `get_build` failure**: output fetch is gated on a
  successful `get_build` (§ Page load and the polling loop; Change 7).
- **E — bounded reads**: structural, in `read_build_output` with the 32 MiB
  clamp and absent-limit default (§ The endpoint contract; Change 1).
- **F — Copy must stay**: Copy and Download coexist; Copy gains a warning
  marker/tooltip when capped (§ Copy and Download both stay).
- **G — viewport jumping**: output fetches freeze while `!following()`, plus one
  final sync on settle (§ Page load and the polling loop; Change 7).
- **H — leading partial line**: stripped once after `align` on tail placements
  (§ Page load and the polling loop; Change 7).
- **I — `line_count` footer**: trimmed footer uses `format_bytes` and counts
  lines in view (§ Footer).
- **5 — allocation churn / `u64`**: `append_capped(&mut String, &str, usize)`
  drains in place, keeping the buffer's allocation (§ The window).
- **Follow-up — exact returned position**: resolved without a header: the body
  is raw bytes and the client computes `requested + raw.len() - back_drop`; the
  `X-Aurcache-Next-Offset` header idea was superseded by client-owned position
  from the byte slice it already holds (§ Client-owned alignment and position).