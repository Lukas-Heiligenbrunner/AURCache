# Review: Configuring workers from the server

Review of [`design/implemented/worker-configuration.md`](../implemented/worker-configuration.md).

## Summary and Verdict

The proposal addresses a genuine operational pain point in AURCache: configuring
worker policy (concurrency, memory/CPU limits, scratch budgets, cache TTLs)
currently requires SSH access to each worker machine, manual edits to `.env` files,
and process restarts that abruptly kill in-flight builds.

**Verdict: Strongly approve the direction and core architectural choices.**
Specifically:
1. **Security boundary:** Refusing to allow the server to push host access
   parameters (`WORKER_BIND_MOUNTS`, `WORKER_BUILD_USER`, `WORKER_MAKECHROOTPKG`,
   `WORKER_CHROOT_DIR`) is critical. The worker runs `devtools` as root via
   passwordless sudo; keeping these keys strictly off the wire preserves the
   isolation boundary where a compromised server cannot escalate to host root on
   workers.
2. **Transport choice (Heartbeat piggyback):** Recommending Option B (piggybacking
   on the existing 15s `/worker/heartbeat`) for Phase 1 is the right call. It
   avoids long-lived stateful connections across replicas, works seamlessly through
   reverse proxies, requires no new dependencies or open ports, and matches the
   actual latency requirements of worker policy changes.
3. **Visibility-first rollout:** Reporting effective configuration and value
   sources back to the server before enabling remote mutations is sound staging.

However, several architectural subtleties, protocol mechanics, and operational
traps in the draft need revision before implementation. This review identifies
those issues and proposes concrete solutions.

---

## High-Priority Concerns & Design Revisions

### 1. The "Environment Wins" Operational Lock-out Trap

The document recommends an **Environment wins** precedence hierarchy:
`worker env > server per-worker > server fleet > built-in default`, arguing:
> "deploying this changes nothing for an existing worker: every value it has today is
> set in its environment and keeps winning."

**The trap:**
In real deployments, `docker-compose.remote-worker.yaml` explicitly sets:
```yaml
WORKER_CONCURRENCY=2
```
And operators running native installations frequently set `WORKER_CONCURRENCY`,
`WORKER_BUILD_MEMORY_MAX`, etc., in `/etc/aurcache/worker.env`.

If environment variables override server settings unconditionally:
1. An operator navigating to the AURCache UI to adjust concurrency or memory limits
   will find those rows permanently locked as "env-pinned (read-only)".
2. The primary motivation stated in the proposal—*"Giving unreal-engine a bigger
   build-tree budget meant an SSH session to the one worker allowed to build it. A
   fleet of five is five edits"*—remains unsolved for any existing deployment until the
   operator SSHes to all five machines to remove those keys from their environment.

**Recommendation:**
Keep local override capability for safety (especially hardware limits), but address
the operational trap:
- **Clean defaults:** Update `docker-compose.remote-worker.yaml` and packaging templates
  so that policy knobs (`WORKER_CONCURRENCY`, limits) are commented out by default,
  leaving only bootstrap variables (`AURCACHE_URL`, `AURCACHE_SERVER_CA_FINGERPRINT`,
  `AURCACHE_ENROLLMENT_TOKEN`, `WORKER_ARCHES`).
- **Server UI clarity:** The UI must clearly indicate why a setting cannot be changed
  (*"Pinned locally by WORKER_CONCURRENCY on the host; remove from host env to manage
  from server"*).
- **Explicit local lock semantics:** Treat an environment variable as a deliberate local
  veto, but document the upgrade path so operators know to clean up their `.env` files
  if they wish to enable centralized fleet management.

---

### 2. Circular "Next Registration" for Priority and Affinity

In Section 5 (*"Applying a change on a running worker"*), the table states:
> | Next registration | priority, package affinity (the server holds these for scheduling, so the worker re-registers when they change) |

This is a circular anti-pattern and conflicts with how the worker lifecycle operates:
1. **The server is the producer and consumer:** Priority and package affinity are
   scheduling decisions evaluated exclusively by the server's job claim logic
   (`aurcache-db/src/helpers/worker_jobs.rs`). The worker does not use priority or
   package affinity to execute builds.
2. **Workers do not re-register while running:** In `aurcache-worker-core::runner::Runner`,
   the worker enrolls once on startup (generating/verifying CSRs and certificates) and
   then loops infinitely between `claim` and background `heartbeat`. There is no
   mechanism or reason for a running worker to initiate a full registration handshake
   just to acknowledge a scheduling preference.
3. **Lag and drift:** If the server holds priority or affinity changes in
   `worker_settings` but requires the worker to re-register before updating
   `workers.priority` and `workers.package_affinity`, the change will never take
   effect until the worker process is manually restarted—reintroducing the very
   restart problem this design set out to eliminate.

**Recommendation:**
- If the server permits configuring priority and package affinity (see Open Question 1
  below), they should take effect on the **next heartbeat acknowledgment**.
- When the worker sends its heartbeat, it acknowledges the new settings. The server then
  updates `workers.priority` and `workers.package_affinity` in the database directly.
  No re-registration should occur.

---

### 3. Separation of Responsibilities: Server Resolves Fleet vs. Worker Overrides

In Section 1 (*"Precedence"*) and Section 2 (*"Recommendation"*), the document suggests:
> "the worker resolves env > worker > fleet > default and applies the result."

Pushing the resolution of `worker` vs. `fleet` down to the worker is unnecessary complexity
and bloats the wire protocol:
- It requires the server to send both the worker-specific overrides and the fleet defaults
  to the worker in `HeartbeatResponse.config`.
- It leaks server-side organizational concepts (fleet defaults) to worker nodes.

**Recommendation:**
The server already possesses both the fleet defaults (`worker_settings WHERE worker_id IS NULL`)
and the per-worker overrides (`worker_settings WHERE worker_id = ?`).
- The **server** resolves `server_config = per_worker_override.unwrap_or(fleet_default)`
  before generating the heartbeat response.
- The **worker** only resolves a simple 3-tier hierarchy:
  `local_env.unwrap_or(server_config).unwrap_or(builtin_default)`.
- The wire payload remains a single flat dictionary/struct of proposed settings.

---

### 4. Dynamic Concurrency Resizing on `tokio::sync::Semaphore`

Section 5 states:
> `concurrency (the claim loop's semaphore is sized once at startup today, so it would add or forget permits -- lowering it lets running builds finish rather than killing any)`

The mechanics of `tokio::sync::Semaphore` make this more delicate than simply calling
`add_permits` or `forget_permits`:
1. In `aurcache-worker-core/src/runner.rs`, permits are acquired via `acquire_owned()`
   and dropped when `run_one` finishes:
   ```rust
   let permit = Arc::clone(&self.permits).acquire_owned().await?;
   tokio::spawn(async move {
       this.run_one(job).await;
       drop(permit); // Calls semaphore.add_permits(1)
   });
   ```
2. If concurrency is 2, both permits are currently acquired (available = 0), and the
   server reduces concurrency to 1:
   - Calling `forget_permits(1)` when available permits is 0 either panics or cannot
     prevent the returning permits from restoring the semaphore to capacity 2 as builds finish.
   - When each running job completes, `drop(permit)` unconditionally returns a permit
     to the semaphore. Without permit-deficit tracking, available permits will return to 2.
3. **Server scheduling race:**
   The server's claim query (`worker_jobs.rs:187`) checks `w.active < w.concurrency`.
   If the server increases concurrency in the DB before the worker has actually resized
   its local concurrency gate, the server will dispatch jobs that the worker has no
   local permits to run.

**Recommendation:**
- Replace raw direct semaphore manipulation with a small `ConcurrencyGate` abstraction on
  the worker that maintains an atomic target and tracks pending permit deficits on release.
- Update `workers.concurrency` in the database only when the worker reports its effective
  concurrency back to the server, ensuring the claim scheduler never races ahead of the
  worker's actual runtime capacity.

---

### 5. Cgroup Controller Enablement Race on Dynamic CPU Limits

In `aurcache-worker/src/executor.rs`:
```rust
let limits = cfg.build_limits;
if !limits.is_empty() {
    if limits.cpus.is_some() {
        hierarchy.enable_cpu()?;
    }
}
```
Currently, `enable_cpu()` writes `+cpu` into `cgroup.subtree_control` **only once at startup**,
and only if `WORKER_BUILD_CPUS` was set in the environment.

If a worker boots without a CPU limit (the default) and the server later configures
`build_cpus = 2.0` dynamically:
- When the next build starts, `BuildCgroup::apply` (`cgroup.rs:106-115`) will attempt
  to write `cpu.max` into the build's cgroup.
- Writing to `cpu.max` in a child cgroup when the parent has not enabled `+cpu` fails
  with `ENOENT` or `ENOSYS`. Because `for_build()` treats limit configuration errors as fatal,
  every subsequent build on that worker will fail immediately.

**Recommendation:**
In `cgroup.rs`, `Hierarchy::for_build` should ensure the required controllers (`cpu`) are
enabled in `subtree_control` on demand before creating child build cgroups, rather than
assuming they were enabled during startup.

---

## Wire Protocol & State Synchronization

### Configuration Versioning & Change Detection

The proposal specifies:
> "B. Heartbeat piggyback. The heartbeat carries the `config_version` the worker has applied;
> when the server's differs, `HeartbeatResponse` includes the new configuration."

To implement this reliably without distributed version counters across server replicas:
1. **Version as Content Digest:**
   Rather than maintaining an incremental integer version in the database (which complicates
   fleet-wide updates where updating a fleet default would require incrementing versions
   for every worker), define `config_version` as the SHA-256 hex digest of the normalized
   server-resolved configuration.
2. **Heartbeat handshake flow:**
   ```mermaid
   sequenceDiagram
   autonumber
   participant W as Worker
   participant S as Server
   participant DB as Database

   Note over W,S: Steady State
   W->>S: POST /worker/heartbeat { active_build_ids, config_version: "a1b2..." }
   S-->>W: 200 OK { cancel: [], config: null }

   Note over S,DB: Admin edits fleet/worker settings in UI
   S->>DB: INSERT/UPDATE worker_settings

   Note over W,S: Next Heartbeat (≤ 15s)
   W->>S: POST /worker/heartbeat { active_build_ids, config_version: "a1b2..." }
   S->>S: Compute resolved config hash ("c3d4...")
   S-->>W: 200 OK { cancel: [], config: { version: "c3d4...", settings: {...} } }

   Note over W: Worker resolves (Env > Server > Default),<br/>updates runtime limits
   W->>S: POST /worker/heartbeat { active_build_ids, config_version: "c3d4...", effective_config: {...} }
   S->>DB: Persist worker effective config & update workers table
   S-->>W: 200 OK { cancel: [], config: null }
   ```
3. **Forward-compatible deserialization:**
   In `aurcache-worker-core/src/client.rs`, `parse_heartbeat_response` discards the entire
   heartbeat body if JSON deserialization fails. If the server introduces a new `WorkerSetting`
   enum variant in a future version, an older worker must not fail to parse `HeartbeatResponse`
   (which would drop the critical `cancel` abort list).
   Use `#[serde(default)]` and a lenient key-value representation for the configuration
   payload to maintain forward compatibility.

### Reporting Effective Configuration & Errors

Section 1 correctly notes that reporting the effective configuration and source of each
key is valuable on its own.
To prevent silent fallback regressions (like the `450G` size fallback to `200G`):
- The `effective_config` payload reported by the worker should include:
  ```rust
  pub struct EffectiveSetting {
      pub value: String,
      pub source: SettingSource, // Env, Server, Default
      pub error: Option<String>,  // Set if the server's value failed validation/application
  }
  ```
- If an operator sets an invalid value or a value that exceeds local host capabilities
  (e.g. asking for 64G RAM on a 16G machine or unparseable duration), the worker falls back
  safely to default/env, but reports `error: Some("...")`. The UI can then flag the worker
  with a warning icon rather than silently pretending everything is fine.

---

## Setting Classification & Allowlist Audit

The table in Section 1 (*"Which settings the server may set"*) is mostly accurate, but has
a few missing keys and edge cases:

| Variable | Proposed Status | Review Judgement | Rationale |
|---|---|---|---|
| `AURCACHE_REPO_HOST`, `AURCACHE_REPO_URL` | *Unlisted* | **No (Worker local)** | These specify how the worker accesses the package repo over the local network/VPN/proxy. Setting them from the server would break workers behind split DNS or NAT. |
| `WORKER_KEYSERVER` | *Unlisted* | **Yes (Policy/tuning)** | Setting the keyserver allows operators to update keyserver URLs fleet-wide when upstream keyservers go down. PKGBUILD `validpgpkeys` verification prevents spoofing. |
| `WORKER_CHROOT_OVERLAY` (`chroot_mode`) | *Unlisted* | **No (Machine fact)** | Determined by filesystem capabilities (e.g. btrfs snapshotting vs ext4 rsync). Forcing overlay mode from the server on unsupported filesystems will cause builds to fail. |
| `WORKER_HEARTBEAT_INTERVAL`, `LEASE_TTL` | *Unlisted* | **No (Protocol bootstrap)** | Must match server's internal `lease_ttl_secs()` and `liveness_timeout_secs()`. Asymmetry between worker and server leads to premature build reaping or zombie builds. |
| `WORKER_POLL_INTERVAL` | *Unlisted* | **Yes (Policy/tuning)** | Safe claim backoff interval. Server may tune this to reduce idle claim load. |
| `mirrorlist` | Listed as policy | **Needs clarification** | `design/implemented/mirrorlist-configuration.md` already implements dynamic per-arch mirrorlists delivered per build. A single worker-level mirrorlist string in `WorkerSetting` conflicts with multi-arch workers (`x86_64` + `aarch64`). Recommend relying on the existing mirrorlist system instead. |

---

## Evaluation of Open Questions

### 1. Package Affinity from the Server
> *"`WORKER_PACKAGES` is policy, but `design/implemented/worker-routing.md` chose worker-declared affinity deliberately: the capability (a key, a toolchain) is on the machine. Should the server be allowed to add a package to a worker's list, or only to show it?"*

**Recommendation:** Allow the server to set affinity per-worker, but **exclude `packages` from fleet defaults**.
- **Why exclude from fleet defaults:** If `packages` were set in fleet defaults, every worker
  without an override would claim affinity for that package, which negates the affinity
  reservation entirely (affinity restricts a package to *only* matching workers).
- **Per-worker configuration is useful:** An operator who provisions credentials on a worker
  host (e.g. mounts an SSH key into `/run/secrets/epic`) should be able to grant the package
  affinity in the UI without restarting the worker container.
- If a package is assigned to a worker that lacks the required host secrets, the build will
  fail—just as misconfiguring memory limits will fail a build. The UI should display a clear
  callout: *"Ensure the worker host possesses the required credentials and build tools for these packages."*

### 2. Activity Log Audit
> *"Configuration changes should appear in the activity log, as package changes do. Per worker or per key?"*

**Recommendation:** Batch changes **per worker per save**, with a separate entry type for
fleet defaults.
- Recording an activity entry per key produces severe log noise when an operator modifies
  multiple fields (e.g. concurrency, memory, builddir budget) in one UI form submit.
- Record: `WorkerConfigUpdated { worker_name: Option<String>, changed_keys: Vec<String> }`.
  When `worker_name` is `None`, it records a change to fleet defaults.

### 3. Drain / Pause
> *"Worth adding to the allowlist in phase 1 as a boolean, or a separate operator action?"*

**Recommendation:** Model drain as a **distinct operator action and column on `workers`**,
NOT as a key in `worker_settings`.
- Drain is an operational lifecycle state (like `ApprovalStatus`), not a tuning configuration.
- A worker should never boot into a drained state via `/etc/aurcache/worker.env`.
- If drain were a setting in `worker_settings`, an accidental fleet-default `drain = true`
  would halt the entire build farm simultaneously.
- When an operator clicks "Drain", the server sets `workers.draining = true`. The server's
  `claim_job` immediately stops offering jobs to that worker. Running builds complete normally.
  The worker does not even need to be aware of the change.

### 4. The Legacy Container Worker (`aurcache-worker-docker`)
> *"`aurcache-worker-docker` reads different variables (`CPU_LIMIT` in milli-CPUs, `MEMORY_LIMIT` in MB). Leave it env-only until it is removed, or map the allowlist onto it?"*

**Recommendation: Leave it env-only.**
`aurcache-worker-docker` is an unmaintained compatibility bridge that is being deprecated.
Translating unit representations (milli-CPUs vs cores, MB vs byte units) and threading remote
config through Docker container executors adds technical debt to code slated for removal.

---

## Database Schema & SQLite Considerations

The suggested storage model:
`worker_settings (worker_id NULL for fleet default, key, value)`

When defining migrations in `aurcache-db`:
1. **Unique Index and SQLite NULL semantics:**
   In SQLite standard behavior, `NULL` values are distinct in unique constraints.
   A standard `UNIQUE(worker_id, key)` will allow duplicate rows where `worker_id IS NULL`.
   To enforce integrity:
   ```sql
   CREATE UNIQUE INDEX idx_worker_settings_fleet
       ON worker_settings(key) WHERE worker_id IS NULL;
   CREATE UNIQUE INDEX idx_worker_settings_worker
       ON worker_settings(worker_id, key) WHERE worker_id IS NOT NULL;
   ```
2. **Foreign Key Cascades:**
   Ensure `FOREIGN KEY (worker_id) REFERENCES workers(id) ON DELETE CASCADE`. When a worker
   is revoked or deleted, its override settings must not be orphaned.
3. **Database Dump and Restore:**
   Add `worker_settings` to `aurcache-utils/src/dump.rs` and `restore.rs`. Fleet defaults
   must be preserved in backup archives. Per-worker overrides must be remapped by worker
   fingerprint upon restore, matching the existing restore logic in `restore.rs`.

---

## Suggested Implementation Roadmap

1. **Step 1: Allowlist & Parser Relocation**
   - Define `WorkerSetting` enum in `aurcache-common`.
   - Ensure all parsing logic (`parse_size`, `parse_duration`) and validation tests in
     `aurcache-common` cover the allowlisted settings.
   - Retire server settings `max_concurrent_builds` and `builder_image` as planned.

2. **Step 2: Effective Config Reporting (Read-only Phase)**
   - Add `effective_config` and `config_version` to `RegisterRequest` and `Heartbeat`.
   - Persist worker-reported configuration in the database.
   - Expose effective config in the Workers UI, displaying the source (`Env`, `Default`)
     and any parsing errors.

3. **Step 3: Database & Server Settings API**
   - Create `worker_settings` table with partial unique indexes.
   - Implement `GET /api/workers/settings/fleet` and `PATCH /api/workers/settings/fleet`.
   - Implement `GET /api/workers/<id>/settings` and `PATCH /api/workers/<id>/settings`.
   - Log batch changes to `aurcache-activitylog`.

4. **Step 4: Dynamic Reconfiguration over Heartbeat**
   - Server resolves `per_worker.unwrap_or(fleet)` and computes hash.
   - `HeartbeatResponse` sends new config when hash mismatches.
   - Worker implements atomic runtime updates:
     - Next build: limits, scratch budget, cache sizes.
     - Immediately: `ConcurrencyGate` (safe semaphore deficit adjustment), chroot refresh interval.
   - Worker reports updated `config_version` on next heartbeat; server updates `workers` table
     routing values (`concurrency`, `priority`, `package_affinity`).
