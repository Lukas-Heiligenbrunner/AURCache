# Build network policy

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

1. Established and related traffic: accept.
2. DNS to the resolvers in the worker's `resolv.conf`: accept.
3. **The allowlist** (below): accept those exact `address:port` pairs.
4. **Local destinations: reject.** That covers:
   - fixed ranges: loopback, 0/8, 10/8, 172.16/12, 192.168/16, 169.254/16,
     100.64/10, 198.18/15 (benchmarking), 240/4 (reserved, with the limited
     broadcast address), fc00::/7, fe80::/10 and multicast. 198.18/15 and
     240/4 follow, in a later PR of this chain; until then the limited
     broadcast address is refused on its own;
   - *(Follows, in a later PR of this chain.)* the NAT64 forms of those IPv4 ranges in the well-known prefix
     `64:ff9b::/96`, and all of the local-use prefix `64:ff9b:1::/48`. Which
     IPv4 address an address in the latter embeds depends on a prefix length
     the network picks (RFC 6052 allows /48 to /96 in it), so it is refused
     whole rather than guessed at;
   - every address of the worker host itself (`fib daddr type local`, which
     also catches the host's public addresses);
   - *(Follows, in a later PR of this chain.)* **the host's links**, read from its main routing table when each build
     starts (`ip -j route`): the prefix of every address on an interface
     (routes of protocol `kernel`), what routers advertise as on-link (`ra`),
     with no gateway; the gateways of its default routes; and the NAT64 forms
     of the IPv4 ones. That is what makes a LAN on public IPv4 addresses, or
     the global IPv6 addresses of a dual-stack LAN (the router, a NAS, the
     server's 2000::/3 address), local;
   - **the repository's host**: every address the name in its URL resolves to
     when the build starts, and the NAT64 forms of the IPv4 ones (with the
     other NAT64 forms, in a later PR of this chain). Rule 3 lets
     the repository's own `address:port` through first, so what this refuses
     is that host's other ports, the server's API and worker protocol among
     them, wherever the host is. Without it a server one router away, on
     another VLAN of a routed IPv6 network say, was open on every port: it is
     on none of the worker's links, and a router's inbound firewall is not on
     the build's path. The cost is a repository behind a CDN or a proxy
     address shared with other sites, whose other ports are refused too.

   The one exception is a listener of the same build on this host: test
   suites that start a server on `127.0.0.1` and connect to it are common, and
   refusing them would fail their builds (see *Telling a build's own listeners
   apart*).

   *(Follows, in a later PR of this chain.)* Rule 4 matches the destination, so the internet stays reachable through
   the gateway: an internet destination is not on a link, and a default route
   names no prefix. Routes of other protocols, a VPN's or the operator's own
   (`boot`, `static`), are left out, because a tunnel that carries the
   internet is routed the same way as one that carries a LAN; a destination
   reached only through one is local only if a fixed range says so. A Docker
   bridge on the host is a `kernel` route like any other, and its prefix is
   private anyway.
5. Everything else: accept.

The one thing the policy knows about the server is its repository's host, which
the build's `pacman.conf` names already; the server's own ports are never
named. A server on the repository's host or on the local network is refused by
rule 4 on every port -- its worker protocol and its API, whatever port they
are published on -- but for what the allowlist names: its repository, and any
exact `host:port` someone chose to allow. Other services on the server's host,
a GitLab beside AURCache on the same NAS, are opened that way one at a time: an
entry for `truenas.lan:30080` opens that port and no other. An entry opens an
address and port rather than a name, so every virtual host a reverse proxy
serves on it is opened with it; the repository's entry included.

Rejections use TCP reset or ICMP unreachable, never a silent drop, so a build
fails at once rather than timing out. The worker lists each refusal with its
destination in the build's log: a rule that refuses something the operator needs should be
obvious to fix.

The policy is on by default. `WORKER_BUILD_FIREWALL=0` still turns it off, for
exotic setups.

### What it still cannot cover

- **An API on the same `address:port` as the repository**, behind one reverse
  proxy (at the same name or another), is reachable wherever the repository
  is. Only authentication fixes
  that, and the documentation says so. The same holds for the worker protocol
  on the repository's `address:port` (one TLS passthrough on 443).
- **An API or worker protocol on an address of its own** that is neither
  local nor the repository's host, such as an API published on the internet
  apart from the repository, is reachable from builds as from anyone on the
  internet: it needs OAuth. A firewall that admits only the workers' addresses
  to its API does not tell builds from workers, since builds connect from
  those same addresses. Tighter rules for this case can come later.
- **A NAT64 translator on a network-specific prefix** translates to whatever
  IPv4 address a build names, private ones included, unless it refuses them
  itself.

## The allowlist

The allowlist is the union of:

| Source | Applies to | Example |
|---|---|---|
| Server, global setting `build_hosts` (`BUILD_HOSTS` env, settings page) | every build | `distcc.lan:3632,flexo.lan:7878` |
| Server, package setting `build_hosts` | that package's builds, **added to** the global list | `gitlab.lan:22` |
| Derived: the `Server =` lines of the build's `pacman.conf`, including the server-supplied mirrorlist | every build | the AURCache repository, a LAN mirror |
| Worker, optional `WORKER_BUILD_ALLOW` (*follows, in a later PR of this chain*) | every build on that worker | a worker on a different LAN with its own distcc helpers |

Entries are `host:port`, with the host a name or an address (IPv6 in brackets).

The worker's policy takes the derived entries; `WORKER_BUILD_ALLOW` follows, in
a later PR of this chain, with the `host:port` parser it and `build_hosts`
share. The
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

**Protocol.** `JobDescriptor` gains an optional `build_hosts: Option<Vec<String>>`,
the resolved server-side union. It is an optional field, so worker protocol 1 is
unchanged. An older worker ignores it; its builds then have no access to those
hosts, which matches that worker's own rules.

## Name resolution

Everything that needs DNS is resolved by the worker **when each build starts**:
the repository's host and the allowlist's. A changed
`gitlab.lan` is therefore picked up by the next build, with no restart and no
refresh timer. A build that runs for hours keeps the addresses it started with.

Every address a name resolves to is allowed. Resolution failure of an
allowlist entry is logged and the entry skipped; the build still runs, and its
log says which entry was left out.

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
socket's cgroup therefore covers every socket a build's processes open, root's
inside the container included, while the worker's own connections to the
server stay outside the rule.

nftables does this with `socket cgroupv2 level N "<path>"`, which matches a
cgroup and all its descendants. The path is resolved to a cgroup id when the rule
is loaded, so a rule naming `build-N` is loaded after that cgroup exists.

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

Those are ruled out by the limits the patched `makechrootpkg`
(`packaging/patch-makechrootpkg.py`) puts on the nspawns it makes. Every one has
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
fill and remove only their own: a fixed name had each worker's startup delete
the others' rules. The table holds:

- **Static, at startup**, on `builds/`: rules 1, 4 and 5, which need no DNS.
  This is the baseline every build gets, even before its own rules load.
- *(Follows, in a later PR of this chain.)* **The links' sets**, `links4` and `links6` (interval sets), with the local
  destinations the host's links make. Shared by every build, and filled afresh
  in each build's transaction.
- **The repository's sets**, `repo4` and `repo6`, with the addresses of the
  repository's host, filled afresh in each build's transaction the same way.
  Apart from the links' sets so that what the host's links make and what the
  repository's name resolves to can change on their own.
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

nftables gives atomic replacement, sets, and one table to remove on shutdown.
The worker depends on `nftables` instead of `iptables`.

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
worker's cgroup namespace (cgroup namespaces are honoured from Linux 6.11). At
startup the worker checks: a process it places in `builds/` (a forked child,
which makes its socket there) connects to a listener of the worker's on
loopback, which the baseline must refuse with a reset. Only `ECONNREFUSED`
counts as held. If it connects, fails some other way, or the check cannot be
made, the policy is not in place.

*Follows, in a later PR of this chain: a fallback that matches the build
user's uid in these cases instead, which holds the build user but not root in
the build's container.*

### Telling a build's own listeners apart

On the way out, nothing says whose socket a local connection will reach. On
the way in it does: in the input hook, `socket cgroupv2` looks up the socket the
packet is for. So a build's packet to a local address is not refused at once;
it leaves with a mark naming the build (`0x41` in the top byte, and the build
id mixed with a number of the worker's below it, set only on packets to
`fib daddr type local`), and the input hook accepts a marked packet only if
the socket it reaches is in that build's cgroup, and refuses it otherwise.

Each table claims only its own builds' marks, and lets any other marked
packet through: with several workers in one network namespace, every table's
input hook sees every worker's builds' packets, and a table that refused the
marks it did not know refused the other workers' builds their own listeners.
The worker's number keeps two workers of different servers, whose builds can
share an id, from claiming each other's.

### Matching the destination before NAT

Rules 2 and 3 match the conntrack original destination (`ct original ip daddr`,
`ct original proto-dst`), the one the build asked for, since NAT in the output
hook runs before the filter: Docker's embedded resolver answers `127.0.0.11:53`
on another port, kube-proxy rewrites a Service's ClusterIP to a pod's address,
and a port Docker publishes on the host is rewritten to the container's.
Rule 4 matches the destination as rewritten, so a published port that leads to
a local address stays refused.

### Reporting refusals

Every refusal in a build's own rules (by cgroup) is added to a dynamic set of
that build's (`address . protocol . port`, at most 1024), in a rule of its own
so a full set never stops the refusal. When the build ends, the worker lists the set, removes
the rules, and appends to the build's log what was refused. No kernel log is involved, so nothing needs reading
`dmesg`.

### Failure

A build whose rules cannot be loaded, or (with the links' sets, in a later PR
of this chain) whose host's routes cannot be read,
is refused. A worker that cannot put the baseline in place at all (no `nft`,
no permission, no `ip`) claims no builds, rather than running them without the
policy: one it claimed would only fail, and be retried until its attempts ran
out, while a worker that could build it waited. It stays online, and logs the
reason at startup and every ten minutes; a build it holds anyway is refused
with that reason.

### Where the derived entries come from

The build's `pacman.conf` (the job's, with AURCache's section appended), the
files it `Include`s as they are on the worker, and the mirrorlist the server
sent. The worker's own files are what counts for an `Include`, because
`arch-nspawn` writes the worker's `extra` mirrors into the chroot's mirrorlist.

### What the tests verify

`scripts/test-kernel.sh` runs `aurcache-worker/tests/build_network.rs` as root
in a privileged container with a private cgroup namespace. Root inside a
`systemd-nspawn` container kept in a build's cgroup (`--keep-unit`, as the
worker keeps it), which is what pacman and the code a dependency leaves for
root run as, is refused the server's worker protocol and API and another local
address, reaches the repository, an allowlisted service on the server's host
and a listener of its own on loopback, and is still refused after moving itself to
the root of its cgroup namespace and of a freshly mounted cgroup2 hierarchy
(both moves are reported, and must succeed). Addresses routed through a host
on a veth link of the worker's, in a network namespace of its own, stay
reachable. (That the host itself is refused on its public IPv4 and global IPv6
addresses, as is the worker's default gateway, follows with the links' sets,
in a later PR of this chain.) The refusals are reported, and the worker
process itself reaches all of them. A repository beyond that host, on a routed address, is reached on
its port while another port of its address is refused and reported, and
another routed address stays reachable. Further tests check that a build
cancelled while its rules load has them loaded and then removed (as `nft
monitor` reports), and that two workers' tables coexist in one network
namespace.

Not tested against the kernel: that two workers matched by cgroup leave each
other's builds' own listeners alone. It needs a second worker's `builds/`
cgroup, and the test process can prepare only its own hierarchy. The rules
that make it so are unit tested: each table's input hook claims only the marks
of its own builds, mixed with its own number.

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
