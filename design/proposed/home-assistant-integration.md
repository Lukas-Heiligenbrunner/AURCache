# Design: Home Assistant, Grafana, and an Outbound Event Bus

Status: **Proposed** · Last updated: 2026-09-30

## Motivation

AURCache knows a lot that an operator wants to see or react to elsewhere: a
build failed, a package went out of date, a worker dropped offline, a new
version landed in the repo. Today none of it leaves the process except as log
lines and as state a browser has to poll `/api/*` for.

Two consumers motivate this:

- **Home Assistant** — surface build/repo/worker state as entities, notify on
  failures, and (later) trigger actions like "add this package" from an HA
  dashboard or automation.
- **Grafana** — dashboards over build history, success rates, durations,
  outdated-package counts, per-package downloads.

These want different transport shapes, so the design is one internal event
source with several thin sinks, plus a metrics surface for the polling
consumers.

---

## Part 1 — The event source

All the interesting transitions already happen in well-defined places. There is
just no outbound seam. The existing `broadcast::channel::<Action>` in
`aurcache/src/main.rs` carries *build commands*, not domain events — it is the
wrong bus to reuse.

### Emit points

| Event | Code location |
|---|---|
| Build enqueued / promoted from `WaitingForDeps` | `aurcache_utils::worker_complete::promote_dependent`, `aurcache-builder/src/init.rs` seeding |
| Build succeeded (+ version, size, worker) | `aurcache_utils::worker_complete::complete_success` |
| Build failed (+ reason, worker) | `aurcache_utils::worker_complete::complete_failure` |
| Package flagged out of date / upstream newer | `aurcache-scheduler/src/update_version_check.rs` |
| Auto-update run started | `aurcache-scheduler/src/auto_update.rs` |
| Worker enrolled / approved / revoked | `aurcache-api/src/worker.rs` |
| Worker online / offline transition | derived from `last_seen` + liveness timeout (already computed for `WorkerSummary.online`) |
| Repo publish (new package version ingested) | `ingest_pkgs`, in the `complete_job` path |

### Shape

New leaf crate `aurcache-events`:

- `enum DomainEvent { BuildSucceeded { pkg, version, worker, size }, BuildFailed
  { pkg, reason, worker }, PackageOutdated { pkg, current, upstream },
  WorkerOnline { .. }, WorkerOffline { .. }, RepoPublished { .. }, … }` —
  `serde`, dependency-free, so any consumer can speak it (same rationale as
  `aurcache-common`).
- A `broadcast::Sender<DomainEvent>` created in `main.rs` and threaded through
  exactly like `tx` / `downloads` / `store` already are.
- Emit calls live **in the owning crates** (`aurcache-utils::worker_complete`,
  `aurcache-scheduler`, `aurcache-api::worker`), never piled into `aurcache/`.
- Sinks are tasks that drain the receiver, spawned from the composition root:
  - **tracing sink** — always on, one structured line per event. Free.
  - **MQTT sink** — feature `mqtt`. See Part 2.
  - **webhook sink** — feature `webhook`, `reqwest` (already a workspace dep).
    POST the event as JSON to a configured URL.
  - **AMQP sink** — feature `amqp`, `lapin`. For operators who already run
    RabbitMQ and want events there directly. Not the Home Assistant path.

A broker or endpoint being unreachable is logged and dropped, never blocks a
build.

The build-log *stream* stays off this bus — it is line-oriented and
high-volume, and that is what the log files and `/worker/jobs/{id}/log` are for.
Publish the transition and the build id, not the log body.

---

## Part 2 — Home Assistant

### Transport: MQTT

Home Assistant has no AMQP integration. Its native paths, most idiomatic first:

1. **MQTT + MQTT Discovery** — publish a retained discovery config once per
   entity and HA auto-creates `sensor.aurcache_builds_failed`,
   `binary_sensor.aurcache_worker_x_online`, etc. This is the "well integrated"
   experience. RabbitMQ's MQTT plugin speaks the same protocol, so RabbitMQ
   users are not excluded — but the integration target is MQTT, not AMQP.
2. **Webhooks** — HA `webhook` trigger. No broker, but every automation is
   hand-written and there are no entities.
3. **REST polling** — HA `rest` sensors against the existing `/api/*`. Zero
   backend work, but poll-latent and hand-configured.

Recommendation: MQTT sink with Discovery. It is the only option that yields real
HA entities automatically.

### MQTT mechanics

- Topic prefix `aurcache/` (configurable). State topics like
  `aurcache/build/<pkg>/state`, `aurcache/worker/<name>/online`,
  `aurcache/repo/<pkg>/version`.
- Publish discovery configs and last-known state with `retain = true` so HA
  repopulates entities after either side restarts.
- Last Will: `aurcache/status` = `offline`, retained; publish `online` on
  connect, so HA marks AURCache unavailable if it drops.

### Authentication

The broker is the auth authority. AURCache and HA are both just clients on it.

**Authentication (who is this client):**

| Mechanism | Notes |
|---|---|
| Username / password | The common case (Mosquitto `password_file`, EMQX built-in). `rumqttc` `MqttOptions::set_credentials`. |
| TLS (`mqtts`, 8883) | Encrypts the link so the password is not on the wire. Do this unless the broker is on localhost/trusted LAN. `rumqttc` + `rustls`; needs system roots or a configured CA bundle. |
| Client certificate (mTLS) | Broker authenticates AURCache by cert (`require_certificate true`). Stronger, more setup. AURCache has mTLS machinery for workers, but the broker CA is a **different trust root** — do not reuse the worker CA. |

**Authorization (what may this client do) — broker ACLs.** This is what contains
the command-path risk in Part 3:

- AURCache user: publish `aurcache/#`, subscribe `aurcache/cmd/#`.
- HA user: subscribe `aurcache/#`, publish `aurcache/cmd/#`.

HA's own `mqtt` integration points at the same broker with its own credentials.
Nothing special on that side.

---

## Part 3 — Actions from Home Assistant

Two ways to get a command into AURCache from HA. Neither needs a custom HA
integration; both use HA built-ins.

### Option A — HA calls the existing REST API (`rest_command`)

`POST /api/package` (`package_add_endpoint`) and Bearer-token auth already exist
and are what the CLI/client use. HA's built-in `rest_command`:

```yaml
rest_command:
  aurcache_add_package:
    url: "https://aurcache.lan/api/package"
    method: POST
    headers:
      authorization: "Bearer {{ token }}"
    content_type: "application/json"
    payload: '{"name": "{{ name }}"}'
```

Zero AURCache changes. No entities — you wire the UI yourself (an `input_text` +
button, or a script). This is the "available today" answer.

### Option B — MQTT command topic

The MQTT sink also *subscribes* to `aurcache/cmd/#`. HA publishes with the
built-in `mqtt.publish` service, or — via Discovery — you expose an MQTT
`button` / `text` entity so "Add package" appears in HA's UI with no YAML.

Consequences:

- The MQTT path becomes bidirectional and can now **mutate state**. It bypasses
  the Bearer token entirely: the authorization boundary is "who can publish to
  `aurcache/cmd/#` on the broker" — i.e. broker ACLs, optionally plus a shared
  secret in the payload.
- The handler must route through the **same** `aurcache_utils::package::add`
  the HTTP endpoint uses — no reimplementation.
- Gate the entire subscribe path behind `AURCACHE_MQTT_COMMANDS_ENABLED`
  (default **off**). Read-only event publishing and state-mutating command
  intake are different risk profiles and must not ship coupled.

### Option C — a HACS custom integration

Config flow for URL + token, entities for builds/packages/workers, services like
`aurcache.add_package`. The "nice" long-term answer, but a separate Python
codebase — realistically a community contribution, not the first thing built.

Recommendation: ship A (nothing to build), add B for the two or three actions
worth having as HA entities (add package, trigger update, retry build), defer C.

---

## Part 4 — Grafana

Grafana is a rendering layer over a data source; it does not require Prometheus.
The question is what stores the numbers.

| Approach | Backend work | New infra | Best at | Weak at |
|---|---|---|---|---|
| Grafana → Postgres/SQLite direct | None | None | Build history, success rates, durations, outdated lists — relational data | Live gauges; couples dashboards to schema |
| Grafana Infinity/JSON plugin → existing `/api/*` | None | None | Current-state panels | Historical trends |
| `/metrics` (Prometheus text) + scraper | ~a screenful | Prometheus (or Grafana Alloy / Cloud) | Live gauges, counters, Grafana Alerting | Per-event granularity between scrapes |
| Push (Pushgateway / remote_write / OTLP) from an event sink | Medium | Pushgateway or OTel Collector | Batch-job semantics, works behind NAT | More moving parts |
| TimescaleDB (or plain Postgres) sink | Small — the Postgres driver is already linked | A Postgres database, Timescale optional | Per-build rows, retention, SQL in Grafana, backfill from history | Another database to run, if one isn't already |
| InfluxDB from an event sink | Medium | InfluxDB | Purpose-built retention | Overkill unless already run; a new client and line protocol |
| Loki + Alloy for build *logs* | None (ship files) | Loki | Log exploration | Not metrics |

Most of what belongs on an AURCache dashboard is relational history already in
the DB with timestamps, not high-frequency instrumentation. So:

- **Grafana pointed straight at the database** is the highest-fidelity,
  lowest-effort path. Native source in Postgres mode; community plugin for
  SQLite (WAL means reads do not fight the writer).
- Add a **`/metrics` endpoint** in `aurcache-api` only for what a DB query
  models badly — queue depth, workers online, repo size/count *now* — and to
  get **Grafana Alerting**. It can run `SELECT count(*) … GROUP BY status` at
  scrape time; the only real in-memory state is counters that must survive
  restart (downloads — already buffered in `DownloadCounter`).

Not redundant, pick by priority:

- Committed to Prometheus elsewhere → `/metrics` only.
- Want rich build analytics with minimal ops → Grafana-on-DB, add `/metrics`
  later for alerting.

Cautions for `/metrics`: no per-package label on a high-churn metric (hundreds
of packages is fine, a combinatorial label explosion is not); keep the
"unknown is `None`, not `0`" convention by omitting a series rather than
exporting a misleading zero.

### Option: a TimescaleDB sink

TimescaleDB is a Postgres extension, which makes it a much cheaper target than
the "a TSDB" row suggests: AURCache already links `sqlx-postgres` through
sea-orm, so writing to it adds no client library, and the same sink works
against a plain Postgres database with Timescale simply absent. It is the
option to take when the operator already runs Timescale (or Postgres) for other
time series and wants AURCache's numbers beside them, queryable in SQL, kept
past AURCache's own retention.

What it offers over `/metrics` and over Grafana-on-DB:

- **Per-build granularity.** One row per finished build — duration, outcome,
  size, peak memory, disk usage, worker, platform, trigger — rather than a
  histogram sampled every scrape interval.
- **Decoupled from AURCache's schema.** Dashboards query a small, documented
  table the sink owns, not `builds`/`packages`, so a migration in `aurcache-db`
  does not break them (the "couples dashboards to schema" weakness of
  Grafana-on-DB).
- **Its own retention.** Deleting a package deletes its `builds` rows; in the
  metrics store they stay for as long as its retention policy says.
- **Backfill.** History already in `builds` can be written on first start;
  a Prometheus scrape only ever sees from the day it was switched on.

**Source: the `builds` table, not the event bus.** Everything a build row in
the metrics store needs is already persisted on `builds` (`start_time`,
`end_time`, `status`, `end_reason`, `size`, `peak_memory`, `disk_*`,
`worker_id`, `platform`, `trigger`). So the sink is one task that reads
finished builds past a watermark (`end_time`, then `id`) and upserts them,
keyed on the build id, with `ON CONFLICT DO NOTHING`. That makes it
at-least-once with idempotent writes: a restart, an unreachable database or a
lagged broadcast receiver costs nothing but delay, and backfill is the same
loop starting from zero. The `DomainEvent` bus, whose broadcast channel drops
on lag, is only a wake-up hint so rows land within a second rather than at the
next poll. The watermark is stored in the metrics database itself, next to the
rows it describes, so pointing the sink at a fresh database re-backfills and
AURCache's own schema gains nothing.

**Gauges** — queue depth by state, workers online, repository size and package
count, out-of-date count — are the same queries `/metrics` would run, sampled
on an interval into a narrow table. Download counts are written as deltas when
`DownloadCounter` flushes, per package (a row per flush is fine in a table; it
is only a *label* that must not explode).

Tables the sink creates, with `CREATE TABLE IF NOT EXISTS`:

```sql
CREATE TABLE aurcache_builds (
    time          timestamptz NOT NULL,  -- end_time
    build_id      bigint      NOT NULL,
    package       text        NOT NULL,
    version       text        NOT NULL,
    platform      text        NOT NULL,
    worker        text,                  -- NULL: built before workers, or unknown
    trigger       text        NOT NULL,
    outcome       text        NOT NULL,  -- success | failure | aborted | ...
    end_reason    text,
    duration_s    double precision,      -- NULL when start_time is unknown
    size_bytes    bigint,                -- all nullable: unknown is NULL, never 0
    peak_memory   bigint,
    disk_chroot   bigint,
    disk_workdir  bigint,
    disk_sources  bigint,
    disk_build_tree bigint,
    PRIMARY KEY (build_id, time)
);
CREATE TABLE aurcache_gauges (
    time  timestamptz NOT NULL,
    name  text        NOT NULL,          -- e.g. queue.waiting, workers.online
    value double precision NOT NULL,     -- unknown: no row, not a zero
    PRIMARY KEY (name, time)
);
CREATE TABLE aurcache_downloads (
    time    timestamptz NOT NULL,
    package text        NOT NULL,
    count   bigint      NOT NULL,        -- delta since the previous flush
    PRIMARY KEY (package, time)
);
```

The primary keys include `time` because a hypertable's unique constraints must.
If `pg_extension` lists `timescaledb`, the sink additionally calls
`create_hypertable(..., if_not_exists => true)` on each table and, when
configured, `add_retention_policy` / compression. Without the extension it
skips those and the tables are ordinary Postgres; nothing else differs.

**This is not AURCache's own database.** It is an external store the operator
names by URL — possibly on the same server, never the same schema. That keeps
Timescale DDL out of `aurcache-db` and its migrations (which stay
backend-agnostic, and would otherwise gain hypertable conversions of `builds`,
whose single-column primary key a hypertable cannot keep). An operator whose
AURCache database *is* Postgres with Timescale installed can still point the
sink at another database on that server.

**Core or plugin?** Under the plugin design's rule this sits on the line: it is
tied to no vendor (it speaks plain Postgres) and costs no dependency, but most
operators would not turn it on. The watermark-over-`builds` shape suits either:
in core it is a scheduler task; as a WASM plugin it needs a socket grant for the
Postgres wire protocol and reads builds over the API with the same cursor.
Proposed: core, behind a cargo feature and off unless the URL is set, decided
together with `/metrics` (see the open question in `plugins.md`).

Cautions: the sink never blocks a build or publish — it is a reader of
committed state with its own task. Write failures are logged and retried from
the watermark. Credentials live in the URL or `PG*` environment, resolved
through `ApplicationSettings`.

---

## Can one endpoint serve both HA and Grafana?

Partly. A single **JSON snapshot endpoint** (extend `/api/stats`, or a new
`/api/metrics.json`) is the pragmatic single surface: HA reads it with `rest`
sensors, Grafana reads it with the Infinity plugin for current-state panels. You
give up historical time series (needs Prometheus in front, which cannot scrape
arbitrary JSON without a `json_exporter` sidecar) and, on the HA side, MQTT
Discovery and push immediacy.

A single **`/metrics`** works for Grafana natively; HA can only consume it via
`command_line` / `rest` templating over the exposition format — no entities, no
device grouping.

What cannot collapse into a GET endpoint: the "HA discovers entities
automatically and reacts within a second of a failure" experience. That is push
+ retained topics + discovery payloads, inherently MQTT-shaped. If that
experience matters, run MQTT for HA and `/metrics` (or DB-direct) for Grafana —
two surfaces, each idiomatic, both fed from the same `DomainEvent` bus.

---

## Configuration surface

All via `ApplicationSettings` (`Package → Env → Global → Default`), never
ad-hoc `env::var`.

| Setting | Default | Purpose |
|---|---|---|
| `AURCACHE_MQTT_URL` | unset (sink off) | e.g. `mqtts://broker:8883` |
| `AURCACHE_MQTT_USERNAME` / `_PASSWORD` | unset | Broker credentials |
| `AURCACHE_MQTT_CLIENT_CERT` / `_KEY` / `_CA` | unset | Optional mTLS / custom CA |
| `AURCACHE_MQTT_TOPIC_PREFIX` | `aurcache` | Topic namespace |
| `AURCACHE_MQTT_COMMANDS_ENABLED` | `false` | Opt in to the `aurcache/cmd/#` write path |
| `AURCACHE_EVENT_WEBHOOK_URL` | unset (sink off) | POST target for the webhook sink |
| `AURCACHE_AMQP_URL` | unset (sink off) | RabbitMQ connection for the AMQP sink |
| `AURCACHE_METRICS_ENABLED` | `false` | Serve `/metrics` |
| `AURCACHE_METRICS_POSTGRES_URL` | unset (sink off) | Timescale / Postgres metrics sink target |
| `AURCACHE_METRICS_GAUGE_INTERVAL` | `60s` | Gauge sampling period for that sink |
| `AURCACHE_METRICS_RETENTION` | unset (keep) | Retention policy, applied only when Timescale is present |

---

## Phasing

1. **`aurcache-events` crate + `DomainEvent` + `broadcast::Sender` threaded
   through `main.rs`, with the tracing sink only.** Wire the
   `BuildSucceeded` / `BuildFailed` / `PackageOutdated` emit points. No external
   dependency, testable in isolation.
2. **`/metrics` endpoint** in `aurcache-api` — smallest standalone win, unblocks
   Grafana + alerting.
3. **MQTT sink with Discovery** (publish only) — the real HA integration.
4. **MQTT command topic** behind `AURCACHE_MQTT_COMMANDS_ENABLED`, routing to
   `aurcache_utils::package::add`.
5. **Timescale / Postgres metrics sink**, when an operator wants per-build
   history outside AURCache's database. Independent of 3–4; needs only the
   gauge queries from 2 and the `DomainEvent` wake-up from 1 (and works
   without it, by polling).
6. Webhook / AMQP sinks as demand appears; HACS integration as a community
   contribution.
