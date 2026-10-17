---
sidebar_position: 4
---

# Workers

```bash
aurcli worker list
aurcli worker approve 3
aurcli worker pause 3                  # stop intake; running builds finish
aurcli worker pause 3 --wait --wait-timeout 7200   # empty it, e.g. before a reboot
aurcli worker resume 3
aurcli worker revoke 3                 # refuse its certificate, requeue its builds
```

Pausing only stops new builds being offered — nothing is sent to the worker and
nothing changes on the machine. Revoking refuses the certificate from that
moment and requeues whatever it was building from the start, so pause first if
a running build is worth keeping. There is no delete: rows are kept so old
builds still name the machine that ran them.

Per-worker tuning without touching the machine:

```bash
aurcli worker config 3                                  # show resolved values and their source
aurcli worker config 3 --set concurrency=2 --set build_memory_max=48G
aurcli worker config 3 --reset build_timeout            # back to the worker's own
```

A value is checked against what that worker accepts before it is saved, and the
worker picks it up on its next heartbeat. See [Worker
Configuration](../workers/split-chroot.md#setting-values-from-aurcache) and
[Managing workers](../workers/managing.md).
