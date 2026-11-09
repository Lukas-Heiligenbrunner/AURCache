---
sidebar_position: 4
---

# Routing builds to specific workers

By default any approved worker can build any package for an architecture it
supports, and AURCache hands out jobs oldest-first. Two settings change that.

## Package affinity — "only this machine can build it"

Some packages can only be built on a particular machine. `unreal-engine` needs
an SSH key belonging to a GitHub account with access to the Epic Games
organisation; another package might need a licensed toolchain, or hundreds of
gigabytes of scratch space. Handing such a job to any other worker does not
degrade gracefully — it fails.

A worker declares what it is provisioned for:

```sh
WORKER_PACKAGES=unreal-engine
```

If a package is named by **any approved worker**, then **only** workers that
name it may build it. Nothing else changes: packages nobody names are still
built by anyone.

Names are exact pkgbase names, comma or space separated. There are no
wildcards — an over-broad pattern would silently reserve packages you never
meant to restrict, and every one of them would stall the moment that machine
went away. Since AURCache's unit is the pkgbase, a split package needs one entry
however many `pkgname`s it produces.

### Affinity ignores whether the worker is online

A reserved package waits for its worker rather than being built elsewhere, even
if that worker is offline. This is deliberate: waiting is recoverable, and
building on a machine without the credential just burns a build and fails.

That means a package can wait indefinitely if its worker never comes back. The
Builds page marks such a build with a warning explaining what it is waiting for,
and **revoking the worker releases the reservation** — only *approved* workers
reserve packages. See [Managing workers](./managing.md).

## Priority — "prefer the fast machine"

The common setup is a slow always-on worker beside the server, plus faster
machines that come and go. `WORKER_PRIORITY` expresses that; higher wins, and
the default of `0` means no preference.

```sh
WORKER_PRIORITY=10     # fast machine
WORKER_PRIORITY=-10    # fallback beside the server
```

Because workers pull work rather than being assigned it, priority is a
hold-back: a lower-priority worker declines a job only while a **strictly**
higher-priority worker could actually take it right now — meaning it is online
and not already at its concurrency limit.

| Situation | What happens |
|---|---|
| Fast worker idle | Fallback holds back; the fast worker takes the job |
| Fast worker at capacity | Fallback takes it **immediately**, no delay |
| Fast worker offline | Fallback takes it immediately |
| Fast worker online but wedged | Fallback takes it after `WORKER_SPILL_DELAY` (60s) |

The last row is a backstop: availability is inferred from heartbeats, so a
worker can look healthy and still never claim anything. The delay bounds that at
one wait per job rather than a stuck queue.

Equal priorities never block each other, so leaving everything at the default
means nothing is ever held back.

## Worked example

```yaml
# beside the server: always up, slow
worker-fallback:
  environment:
    WORKER_PRIORITY: "-10"

# elsewhere on the LAN: fast, sometimes off
worker-fast:
  environment:
    WORKER_PRIORITY: "10"
    WORKER_CONCURRENCY: "16"

# holds the Epic Games SSH key
worker-epic:
  environment:
    WORKER_PRIORITY: "10"
    WORKER_PACKAGES: "unreal-engine"
```

`unreal-engine` only ever goes to `worker-epic`. Everything else prefers the
fast workers and falls to `worker-fallback` when they are busy or absent — with
no delay in either case, because the fallback only waits while a faster worker
is genuinely able to take the job.

## The two are independent

Affinity decides **who may**; priority decides **who first**. Priority never
overrides affinity: a faster worker that lacks the affinity cannot take the job
and does not hold it back from a worker that has it.
