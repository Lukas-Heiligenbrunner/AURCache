# Design: Build capabilities

Some packages need more than the build sandbox grants. AUR `wstunnel`'s test
suite makes a TUN device, which takes `CAP_NET_ADMIN`; without it the build
fails in `check()`, and `--nocheck` is the only way through. This lets a
package ask for Linux capabilities and a worker grant them.

User documentation: `docs/docs/workers/capabilities.md`.

## Shape

| Piece | Where | What |
|---|---|---|
| `Capability`, `Capabilities` | `aurcache-common/src/capability.rs` | The 41 kernel capabilities by name; parsing (any case, `CAP_` optional), normalising, and `breaks_confinement` |
| `build_capabilities` | `Setting::BuildCapabilities`, scope `PackageOnly` | What a package asks for. Validated on write: known names, none that breaks confinement |
| `JobDescriptor::capabilities` | `aurcache-common/src/worker.rs` | The request, as names. Optional, `skip_serializing_if` empty |
| `WORKER_BUILD_CAPABILITIES` | `aurcache-worker/src/capabilities.rs`, `Config` | What this worker allows. Environment only, empty by default |
| `grant` | same | Every capability asked for is allowed, or the build is failed before anything is fetched |
| `AURCACHE_BUILD_CAPABILITIES` | `job.rs` → `packaging/patch-makechrootpkg.py` | What the build script hands the build |
| `refusal` | `aurcache-worker-docker/src/executor.rs` | The legacy container worker fails any build that asks |

## The worker decides

The package setting is a request; the worker's allowance is the grant. The
allowance is not a declared worker setting, for the reason
`WORKER_BIND_MOUNTS` is not (`worker-configuration.md`, "What a worker must
not declare"): it decides what a build may reach, and a server that could set
it -- compromised, or merely mistaken -- could hand any package any
capability. A test in `aurcache-worker/src/settings.rs` holds it off the
declared list.

Ten capabilities are refused on both sides whatever is configured
(`Capability::breaks_confinement`): `CAP_SYS_ADMIN`, `CAP_SYS_MODULE`,
`CAP_SYS_RAWIO`, `CAP_SYS_PTRACE`, `CAP_SYS_BOOT`, `CAP_DAC_READ_SEARCH`,
`CAP_BPF`, `CAP_PERFMON`, `CAP_MAC_ADMIN`, `CAP_MAC_OVERRIDE`. Each reaches
the kernel or the host directly -- the set container runtimes withhold for
that reason. The server refuses them as a package value, so nobody stores a
request no worker will meet, and a worker refuses to start with one in its
allowance, rather than dropping it, so the operator learns at once.

`CAP_NET_ADMIN` is deliberately not among them, though it is not harmless:
chroot builds share the worker's network namespace, so it reaches the worker's
own interfaces, routes and firewall -- including the rule that keeps builds
off the worker-protocol port (`build_firewall.rs`). Giving builds a network
namespace of their own would contain it, and would also cut every build off
the network it fetches from unless the worker set up a bridge, which is
configuration no worker has today. So it is the operator's call, made on the
worker, and the documentation says what it exposes.

Only per package, with no server-wide value and no environment variable
(`Scope::PackageOnly`): a capability is a decision about one package's
PKGBUILD. `settings::write` refuses a server-wide value, and resolution never
reads one.

## Reaching the build user

Adding a capability to the container is not enough, and is the half that is
easy to get right. `systemd-nspawn --capability=` keeps it in the container's
bounding set, but makepkg runs as `builduser`, reached through `sudo -iu`, and
the kernel clears every capability when a process changes from root to
another user. sudo cannot carry it either: it is setuid, and executing a
setuid program clears ambient capabilities.

So the build script (interface 2) does both halves, and only when
`AURCACHE_BUILD_CAPABILITIES` is set:

1. `nspawn_build_args` gains `--capability=<list>` and
   `--setenv=AURCACHE_BUILD_AMBIENT=+net_admin,...` -- on the build's
   container only, not those that install or upgrade its packages.
2. `_chrootbuild` runs makepkg through `setpriv --reuid=builduser
   --regid=builduser --init-groups --inh-caps=... --ambient-caps=...
   --reset-env` and a login shell, instead of `sudo -iu`. Ambient
   capabilities survive the user change and every later `execve` of an
   ordinary program, so `prepare()`, `build()`, `check()` and `package()` all
   hold them. setpriv is util-linux, which every `base-devel` chroot has.

A build granted nothing takes the stock `sudo` path, unchanged.

The interface moves to 2 because a worker that sets the variable relies on a
copy that reads it; an interface-1 copy would build without the capabilities,
silently. `scripts/test-sandbox.sh` checks both ways on a real nspawn build: a
fixture whose `check()` makes a TUN device succeeds granted and is refused
withheld.

No device is mapped. systemd-nspawn already gives every container
`/dev/net/tun`; what the build lacked was the capability to use it.

## Protocol

`JobDescriptor::capabilities` is an optional field within protocol 1: absent
is none, which is what every committed fixture reads as
(`tests/worker_protocol.rs`). Names rather than an enumeration, so a
capability one side does not know is refused by name instead of failing the
whole job to decode.

A worker that predates the field ignores it and builds without the
capabilities. For a package that needs them that is the failure it already
had without the setting, and nothing worse.

A refused build is reported `failed`, not `interrupted`: the same worker would
refuse it again, and a retry would only spend its attempts.

## Not done: routing

The server does not know which workers allow what, so any capable worker may
claim a build that asks for capabilities, and one that does not allow them
fails it. In a mixed fleet, `WORKER_PACKAGES` on the allowing worker routes
those packages to it (affinity makes it their only builder).

Routing on it properly means the worker reporting its allowance at
registration (an optional `RegisterRequest` field, so no protocol change), a
column for it, `Fleet::capable` reading each queued build's request, and a
waiting reason for a request no approved worker meets. Left for when a fleet
needs it.
