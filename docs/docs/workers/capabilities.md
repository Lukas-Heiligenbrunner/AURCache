---
sidebar_position: 5.5
---

# Build capabilities

A build runs as an unprivileged user in a container, which is enough for
nearly every package. A few test suites need more: AUR `wstunnel`'s, for
example, makes a TUN device, which takes `CAP_NET_ADMIN`. Without it the build
fails in `check()` with `Operation not permitted`, and the only way through is
`--nocheck`.

Such a package can ask for the capabilities it needs, and a worker grants them
if its operator allows it. Both are needed: the package's setting is a
request, and the worker decides.

## On the package

Set **Capabilities** in the package's build config (the `build_capabilities`
setting) to the names it needs, comma separated:

```
CAP_NET_ADMIN
```

Names are those of capabilities(7), in any case, with or without the `CAP_`
prefix; `net_admin` is `CAP_NET_ADMIN`. The setting exists only per package:
there is no server-wide value and no environment variable, because what a
capability lets a build do is what that package's PKGBUILD may then do on the
worker.

Some capabilities are refused outright, here and on every worker, because each
reaches past any container to the machine itself: `CAP_SYS_ADMIN`,
`CAP_SYS_MODULE`, `CAP_SYS_RAWIO`, `CAP_SYS_PTRACE`, `CAP_SYS_BOOT`,
`CAP_DAC_READ_SEARCH`, `CAP_BPF`, `CAP_PERFMON`, `CAP_MAC_ADMIN` and
`CAP_MAC_OVERRIDE`. A package that needs one of them needs a machine of its
own.

## On the worker

A [chroot worker](./split-chroot.md) grants what `WORKER_BUILD_CAPABILITIES`
names, and nothing by default:

```
WORKER_BUILD_CAPABILITIES=CAP_NET_ADMIN
```

It is read from the worker's own environment only. Like `WORKER_BIND_MOUNTS`,
it decides what a build may reach, so AURCache cannot set it from the
worker's page: a server that could would be able to hand any package any
capability. A value the worker cannot grant -- a name that is not a
capability, or one of those refused above -- stops it from starting.

A build that asks for a capability its worker does not allow fails before it
starts, saying which one and what to set:

```
[worker] this build asks for CAP_NET_ADMIN, which this worker does not grant.
Allow it on the worker (WORKER_BUILD_CAPABILITIES=CAP_NET_ADMIN), or remove it
from the package's build_capabilities setting.
```

A build that is granted capabilities says so at the top of its log, and its
build user holds them for the whole of `makepkg`: `prepare()`, `build()`,
`check()` and `package()`.

:::warning
A granted capability is held by the package's PKGBUILD, and by everything it
runs. Builds share the worker's network namespace, so `CAP_NET_ADMIN` reaches
the worker's **own** network: its interfaces, routes and firewall, including
the rule that keeps builds off the worker-protocol port
(`WORKER_BUILD_FIREWALL`). Allow it on a worker whose network you are prepared
to let those packages change, and route only those packages to it.
:::

### Which worker builds it

The server does not yet know which workers allow which capabilities, so any
worker may claim a build that asks for some. In a fleet where only some
workers allow them, send those packages to those workers with
[`WORKER_PACKAGES`](./routing.md): a package one worker claims affinity for is
built by that worker alone.

### Other workers

- The [legacy container worker](./legacy-docker.md) grants none, and fails a
  build that asks for any. Its builder image runs makepkg as an unprivileged
  user, whom a capability added to the container would never reach.
- A worker older than this setting does not read it, and builds without the
  capabilities, where a build that needs them fails as it did before.

Devices are not part of this. A chroot build's container already has
`/dev/net/tun`; what it lacked was the capability to use it.
