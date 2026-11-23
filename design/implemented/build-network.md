# Build network policy

Status: **Implemented** for the chroot worker; the Docker and Kubernetes
executors and the follow-ups are not built · Last updated: 2026-11-23

What a build may connect to, on every executor.

## Problem

A build runs code from people other than the operator: the PKGBUILD itself, and
whatever its AURCache dependencies leave for root to run. Their install
scriptlets and alpm hooks run with no network (`aurcache-sandbox offline`), but
what they leave in the chroot, a file in `/etc/profile.d` say, runs as root with
the build's network as the build starts. That code can reach whatever the worker
can. Two kinds of destination need protecting:

- **The server.** The worker protocol (8083 by default) accepts a rogue
  registration. The web UI and API (8080) serve anyone who reaches them when
  OAuth is not configured, so a build could add, delete or patch packages.
- **The worker's local network.** Routers, NAS admin pages, databases,
  unauthenticated services, and the worker host's own listeners.

The chroot worker's current firewall rejects the build user's TCP connections to
the server's ports, on every host. That has three holes:

- a port reached under another number (a Docker mapping `18080:8080`, a proxy)
  is not covered;
- root inside the chroot (pacman, and what a dependency left for root to run) is
  not covered, since the rule names a uid;
- the rest of the local network is not covered at all.

## Policy

Builds may reach the internet, and on the local network only what is allowed
explicitly. For each address family, in order:

1. Established and related traffic: accept. Related includes the connections
   a conntrack helper (FTP's, SIP's) expects. The kernel assigns no helper by
   itself (`nf_conntrack_helper` is off by default since Linux 4.7), so this
   only matters on a host whose own ruleset assigns one: there, a server that
   a build may reach could have it admitted to further connections that the
   server names.
2. DNS to the resolvers in the worker's `resolv.conf`: accept.
3. **The allowlist** (below): accept those exact `address:port` pairs.
4. **Local destinations: reject.** That covers:
   - fixed ranges: loopback, 0/8, 10/8, 172.16/12, 192.168/16, 169.254/16,
     100.64/10, 240/4 (reserved, with the limited broadcast address),
     fc00::/7, fe80::/10 and multicast;
   - every address of the worker host itself (`fib daddr type local`, which
     also catches the host's public addresses);
   - **the host's links**: the prefix of every address on its interfaces,
     and of a point-to-point link's peer, read with `getifaddrs`, however the
     address or its route was made (a DHCP client's `proto dhcp` route,
     `noprefixroute`, a VRF). That is what makes a LAN on public IPv4
     addresses, or the global IPv6 addresses of a dual-stack LAN (the router,
     a NAS, the server's 2000::/3 address), local;
   - **the server's hosts**: every address that the repository's host
     resolves to at startup or when a build starts, and every address of the
     host that the worker dials for the worker protocol (`AURCACHE_URL`).
     Rule 3 lets the repository's own `address:port` through first, so what
     this refuses is those hosts' other ports, the server's API and worker
     protocol among them, wherever the hosts are. Without it, a server one
     router away (on another VLAN of a routed IPv6 network, say) would be open
     on every port: it is on none of the worker's links, and a router's inbound
     firewall is not on the build's path. The cost is a server behind a CDN or
     a proxy address shared with other sites, whose other ports are refused
     too.

   The one exception is a listener of the same build on this host: test
   suites that start a server on `127.0.0.1` and connect to it are common, and
   refusing them would fail their builds (see *Telling a build's own listeners
   apart*).

   Rule 4 matches the destination, so the internet stays reachable through
   the gateway: an internet destination is in no prefix of the host's
   addresses. Routes are not read: an IPv4 gateway is in the prefix of an
   address, and an IPv6 one is link-local, which a fixed range covers.
5. Everything else: accept.

The one thing the policy knows about the server is its hosts: the repository's,
which the build's `pacman.conf` names already, and the one the worker dials; the
server's own ports are never named. A server on those hosts or on the local
network is refused by rule 4 on every port -- its worker protocol and its API,
whatever port they are published on -- but for what the allowlist names: its
repository, and any exact `host:port` someone chose to allow. Other services on
the server's host, a GitLab beside AURCache on the same NAS, are opened that way
one at a time: an entry for `truenas.lan:30080` opens that port and no other. An
entry opens an address and port rather than a name, so every virtual host a
reverse proxy serves on it is opened with it; the repository's entry included.

Rejections use TCP reset or ICMP unreachable, never a silent drop, so a build
fails at once rather than timing out. The worker lists each refusal with its
destination in the build's log: a rule that refuses something the operator needs
should be obvious to fix.

The policy is on by default. `WORKER_BUILD_FIREWALL=0` still turns it off, for
exotic setups.

### What it still cannot cover

- **An API on the same `address:port` as the repository**, behind one reverse
  proxy (at the same name or another), is reachable wherever the repository
  is. Only authentication fixes
  that, and the documentation says so. The same holds for the worker protocol
  on the repository's `address:port` (one TLS passthrough on 443).
- **An API on an address of its own** that is neither local nor one of the
  server's hosts, such as an API published on the internet apart from the
  repository and the worker protocol, is reachable from builds as from anyone on
  the internet: it needs OAuth. A firewall that admits only the workers'
  addresses to its API does not tell builds from workers, since builds connect
  from those same addresses. Tighter rules for this case can come later.
- **A NAT64 translator** translates to whatever IPv4 address a build names,
  private ones included, unless it refuses them itself.

## The allowlist

The allowlist is the union of:

| Source | Applies to | Example |
|---|---|---|
| Server: global `build_hosts` (`BUILD_HOSTS`, settings page) | every build | `distcc.lan:3632` |
| Server: package `build_hosts` | its builds, **added to** the global list | `gitlab.lan:22` |
| The `Server =` lines of the build's `pacman.conf` and mirrorlist | every build | the repository |
| Worker: `WORKER_BUILD_ALLOW` | that worker's builds | its own distcc helpers |

Entries are `host:port`, with the host a name or an address (IPv6 in brackets,
without a zone: the rules match an address, not an interface). An entry
opens its port to TCP and UDP alike.

The worker's policy takes the derived entries and `WORKER_BUILD_ALLOW`. The
server's `build_hosts` setting, with the `JobDescriptor` field below that
carries it, is implemented separately, by #456, which follows it; until then
the two `build_hosts` rows above, the resolution rule and the protocol change
below are design only.

**Server-side lists are fine.** Workers and the server share a local network in
nearly every deployment, especially for private services. The server already
makes the same assumption when it hands workers their mirrorlist.

**`build_hosts` resolves as a union, not by precedence.** Settings normally
resolve `Package -> Env -> Global -> Default`, the first one set winning. For
this key the package's list is added to the global one (env or settings page),
so a package that needs one more host does not have to repeat the global list.

**Protocol.** `JobDescriptor` gains an optional field,
`build_hosts: Option<Vec<String>>`, the resolved server-side union, so worker
protocol 1 is unchanged. A worker that predates the field ignores it: one
without this policy (0.6.0's) lets its builds reach those hosts anyway, as it
lets them reach the whole local network, and one with the policy refuses them.

## Name resolution

Everything that needs DNS is resolved by the worker **when each build starts**:
the server's hosts and the allowlist's. A changed
`gitlab.lan` is therefore picked up by the next build, with no restart and no
refresh timer. A build that runs for hours keeps the addresses it started with.

Every address a name resolves to is allowed. Resolution failure of an
allowlist entry is logged and the entry skipped; the build still runs, and its
log says which entry was left out.

The server's hosts are resolved, and the host's links read, at startup as
well, which must succeed: a worker that fails at either takes no builds.
After that, each build only adds to the local sets, which every build shares:
a lookup or a reading that fails or comes back short adds nothing and takes
nothing away, so it cannot reopen anything to the builds running. The cost is
that an address that the server's hosts no longer have, or a prefix that the
host no longer has, stays refused until the worker restarts.

## Enforcement on the chroot worker

### Match the build's cgroup, not its uid

The worker already places each build in a cgroup of its own (`cgroup.rs`):

```text
aurcache-worker.service/
├── worker/        the worker process itself
└── builds/        every build
    └── build-N/   makechrootpkg -> systemd-nspawn -> pacman, hooks, makepkg
```

The patched `arch-nspawn` keeps the container in that cgroup. Matching the
socket's cgroup therefore covers every socket that a build's processes open,
root's inside the container included, while the worker's own connections to the
server stay outside the rule.

nftables does this with `socket cgroupv2 level N "<path>"`, which matches a
cgroup and all its descendants. The path is resolved to a cgroup id when the
rule is loaded, so a rule naming `build-N` is loaded after that cgroup exists.

### Root in the build's container

Root runs in every build's container: pacman, the install scriptlets, and
whatever a dependency left for root to run there, such as a file in
`/etc/profile.d` that the build script's login shell sources. The cgroup match
holds root's ordinary sockets like any other process's. It cannot see two
things root could otherwise do:

- **raw frames**, through an `AF_PACKET` socket (`CAP_NET_RAW`), which bypass
  the IP hooks the rules are on;
- **sockets the kernel opens itself**, for a network filesystem mount (an NFS
  mount, with `CAP_SYS_ADMIN`) or a key request's upcall, which are in no
  build's cgroup.

The patched `makechrootpkg` (`packaging/patch-makechrootpkg.py`) rules those
out with limits on every nspawn that it makes. Every one has
`--drop-capability=CAP_NET_RAW` and a system call filter that denies the keyring
calls. The build's container drops `CAP_SYS_ADMIN`, so root there cannot mount a
network filesystem; the others keep it, and their filter also denies `@mount`.
This policy relies on them. The build user has no way to root of its own (no
sudo rule, and `no_new_privs`), and the dependencies' scriptlets run in a
sandbox without network (`aurcache-sandbox offline`), so what root runs with the
build's network is what a dependency left for it as the build starts.

`AF_VSOCK` is outside this policy as well: no network namespace confines it
and nftables cannot filter it, so on a worker in a virtual machine a build
could reach the hypervisor's vsock services and vsock listeners in the worker
VM itself. The install's sandbox already refuses every socket but a UNIX one;
#461 refuses `AF_VSOCK` in every container of a build and in the PKGBUILD's
runs outside the chroot.

### Layout

One table per worker, `inet aurcache_worker_<id>`, named after the first 16
hex digits of its identity's fingerprint. Workers sharing a network namespace
(containers on the host's network, several units on one machine) each make,
fill and remove only their own: a fixed name would have each worker's startup
delete the others' rules. A table left by an earlier run matches the cgroups
of that run's builds; once those are gone, as when the container or the unit
restarts, it holds nothing. With `WORKER_BUILD_FIREWALL=0` a worker removes
its own table anyway, so a `builds/` that outlived its last run is not held to
that run's baseline. The table holds:

- **Static, at startup**, on `builds/`: rules 1, 4 and 5, which need no DNS.
  This is the baseline every build gets, even before its own rules load.
- **The local sets**, `local4` and `local6` (interval sets that merge what
  is added to them), with the prefixes of the host's links and the addresses
  of the server's hosts: filled at startup, and only added to by each
  build's transaction, which reads the links and resolves the hosts again.
  A link or an address gone since stays refused until the worker restarts.
  `nft` works a merge out from its own copy of a set, so the worker makes
  every change to its table under one lock: two builds starting at once
  would otherwise delete what the other had already merged.
- **Per build**, on `build-N`, loaded atomically (`nft -f`) after the cgroup is
  created and before the build starts: rules 2 and 3, with the addresses
  resolved for that build, and rule 4 again, so that the build's own
  listeners can be let through and its refusals recorded as its own. Removed
  when the build ends.

If a build's rules fail to load, the build is refused with that reason; it never
runs with the baseline alone, which lacks the allowlist pacman needs. The rules
come back from the blocking task that loads them inside the guard that
removes them, so a build whose future is dropped mid-load still has them
removed: the task runs to the end, and its unclaimed result is dropped.

nftables gives atomic replacement, sets, and a table of each worker's own, made
afresh at startup. The worker depends on `nftables` instead of `iptables`. A
native worker upgraded from a release that used `iptables` keeps the rule
inserted by that release (the build user refused the worker-protocol port, in
`OUTPUT`) until the host reboots; it refuses nothing the policy allows, and the
docs say how to remove it.

### Where builds cannot be matched by cgroup

The policy holds a build by its cgroup, and nothing else tells a build from
the worker. Where builds cannot be matched that way, the policy cannot be put
in place, and the worker takes no builds (see *Failure*), naming the reason.
That is:

- no cgroup hierarchy (a container that is not privileged, a unit without
  `Delegate=yes`): no build cgroup exists to match;
- a builds' cgroup whose path nftables cannot be given: its quoted strings take
  every character literally, a backslash included (systemd's `\x2d`, as in
  `system-aurcache\x2dworker.slice`), but have no escape for `"`;
- a baseline on the cgroup that nftables refuses: `socket cgroupv2` needs
  Linux 5.13;
- the startup check below, when it connects or cannot be made.

The same applies where the kernel does not match `socket cgroupv2` from the
worker's cgroup namespace. The kernel resolves the rule's path in that
namespace, but matched the level from the host's root until 7f3287db6543
("netfilter: nft_socket: make cgroupsv2 matching work with namespaces"), in
Linux 6.11 and backported to stable kernels (6.6.53 among them). At startup
the worker checks: a process it places in `builds/` (a forked child, which
makes its socket there) connects to a listener of the worker's on loopback,
which the baseline must refuse with a reset. Only `ECONNREFUSED` counts as
held. If it connects, fails some other way, or the check cannot be made, the
policy is not in place.

A container with no cgroup namespace of its own has no such mismatch: the
worker sees the host's hierarchy and names its cgroups from the host's root,
the root from which an older kernel counts levels. So the shipped compose files
run chroot workers with `cgroup: host` (`--cgroupns=host`), which costs nothing
that a privileged container did not already have: such a container can mount the
host's hierarchy anyway, and each build's container still gets a cgroup
namespace of its own from `systemd-nspawn`. The build user sees more, though:
`makepkg --printsrcinfo` runs in the worker's container and can read the
host's whole cgroup tree there, as on a native install. Where the namespace
cannot be left, as in a Kubernetes pod, the kernel has to carry the fix.

The worker's log says which of these to do: leave the cgroup namespace, run a
kernel with the fix, or set `WORKER_BUILD_FIREWALL=0` and accept that builds
reach the server and the local network.

There is no fallback that matches the build user's uid instead. It would hold
the build user and not root in the build's container, which is where pacman
runs and where code left by a dependency runs as the build starts: the hole this
policy exists to close. A worker that seemed protected would be worse than
one that says it is not; and its refusals, shared by every build of the user,
could not be reported per build, nor a build's own test listeners told apart.

### Telling a build's own listeners apart

On the way out, nothing says whose socket a local connection will reach. On
the way in it does: in the input hook, `socket cgroupv2` looks up the socket
that the packet is for. So a build's packet to a local address is not refused at once;
it leaves with a mark naming the build (`0x41` in the top byte, and the build
id below it, set only on packets to
`fib daddr type local`), and the input hook accepts a marked packet only if
the socket it reaches is in that build's cgroup, and refuses it otherwise.

Each table claims only its own builds' marks, and lets any other marked
packet through: with several workers in one network namespace, every table's
input hook sees every worker's builds' packets. A table that refused unknown
marks would refuse other workers' builds their own listeners.
Workers of different servers, whose builds can share an id and so a mark,
need network namespaces of their own.

### Matching the destination before NAT

Rules 2 and 3 match the conntrack original destination (`ct original ip daddr`,
`ct original proto-dst`), the one the build asked for, since NAT in the output
hook runs before the filter: Docker's embedded resolver answers `127.0.0.11:53`
on another port, kube-proxy rewrites a Service's ClusterIP to a pod's address,
and a port that Docker publishes on the host is rewritten to the container's.
Rule 4 matches the destination as rewritten, so a published port that leads to
a local address stays refused.

### Reporting refusals

Every refusal in a build's own rules (by cgroup) is added to a dynamic set of
that build's (`address . protocol . port`, at most 1024), in a rule of its own
so a full set never stops the refusal. When the build ends, the worker lists the
set, removes the rules, and appends to the build's log what was refused, with a
pointer to `WORKER_BUILD_ALLOW`. No kernel log is involved, so nothing needs
reading `dmesg`.

### Failure

A build whose rules cannot be loaded is refused. A worker that cannot put the
baseline in place at all (no `nft`, no permission, no match by cgroup, the
server's hosts not resolving at startup) claims no builds, rather than running
them without the policy: one it claimed would only fail, and be retried until
its attempts ran out, while a worker that could build it waited. It stays
online, and logs the reason at startup and every ten minutes; a build it holds
anyway is refused with that reason.

### Where the derived entries come from

The build's `pacman.conf` (the job's, with AURCache's section appended), the
files it `Include`s as they are on the worker, and the mirrorlist the server
sent. The worker's own files are what counts for an `Include`, because
`arch-nspawn` writes the worker's `extra` mirrors into the chroot's mirrorlist.

### What the tests verify

`scripts/test-kernel.sh` runs `aurcache-worker/tests/build_network.rs` as root
in a privileged container with a private cgroup namespace. The probe is root
inside a `systemd-nspawn` container kept in a build's cgroup (`--keep-unit`, as
the worker keeps it), as pacman is, and code that a dependency leaves for root.
It is refused the server's worker protocol and API and another local address,
reaches the repository and a listener of its own on loopback, and is still
refused after moving itself to the root of its cgroup namespace and of a
freshly mounted cgroup2 hierarchy (both moves are reported, and must succeed).
A host on a veth link of the worker's, in a network namespace of its own, set
up after the worker started, is refused on its public IPv4 address, its
global IPv6 address, the peer address of an address of the worker's, and an
address in a prefix of the worker's that has no kernel route
(`noprefixroute` with a route of protocol `dhcp`, as dhcpcd makes them),
while addresses routed through that host stay reachable. The refusals are
reported, and the worker process itself reaches all of them. A repository beyond that host, on a
routed address, is reached on its port while another port of its address is
refused and reported, and another routed address stays reachable; a worker
protocol on a routed address is refused on every port while the repository is
elsewhere. Further tests check that a build cancelled while its rules load has
them loaded and then removed (as `nft monitor` reports), that a build opened
while another runs leaves that one refused the server's other ports, and that
two workers' tables coexist in one network namespace.

The script runs them a second time in a container with the host's cgroup
namespace (`--cgroupns=host`), as the compose files run the worker.

`scripts/test-e2e.sh` checks the worker image itself: while the test build
runs, a probe joins that build's cgroup and checks that root and the build user
are refused `aurcache:8080` and `aurcache:8083` and reach `aurcache:8081`, with
the build's real rules; then that the build's log reports the refusals. The
worker's startup check covers `socket cgroupv2` matching in that image.

## Other executors

The same policy, each enforced where the build cannot reach it:

- **Docker worker**: rules in `DOCKER-USER`, matched on the build network's
  subnet. They sit outside the container, so root inside it cannot remove them,
  which the current uid rule could not offer.
- **Kubernetes**: a `NetworkPolicy` with egress to `0.0.0.0/0` and `::/0` minus
  the local ranges, plus the allowlist. It needs a network plugin that enforces
  policies.

These follow in their own PRs, after the chroot worker's.

## Follow-ups

- **AURCache as a caching pacman proxy.** With the repository port proxying the
  official repositories, a build's `pacman.conf` would need only one local
  destination. It mostly helps Docker and Kubernetes, where pacman's traffic is
  filtered. A design of its own.
- **distcc as a worker setting.** `WORKER_DISTCC_HOSTS` written into
  `makepkg.conf`, with its hosts derived into the allowlist. Until then,
  `BUILD_HOSTS` plus the worker's own `makepkg.conf` work. The documentation
  notes that a reachable distccd runs any build's compilers, so its helpers
  should be dedicated machines with `--allow` set to the workers.
