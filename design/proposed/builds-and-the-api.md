# Keeping builds away from the API, and the LAN

Status: **Proposed** · Last updated: 2026-10-05

## The problem

A build runs a stranger's code, and it has the network: it must, to fetch
sources. It can therefore reach the server's HTTP API, and what that gets it
depends on authentication:

- **OAuth on**: nothing. The `Authenticated` guard
  (`aurcache-api/src/models/authenticated.rs`) wants a session cookie or an
  API token, and a build has neither.
- **OAuth off** -- upstream's default, and what most single-container
  deployments run: everything. The guard admits every request, so a build can
  do whatever an admin can. The worst of it is not deleting packages but
  **patching another package**: the patch lands in a package people install,
  and the next build of it ships the build's code. One malicious PKGBUILD
  becomes a persistent supply-chain foothold.

Where builds reach the API from:

| setup | how | address |
|---|---|---|
| hybrid, either mode | the build shares the server's network namespace (`container:<id>`, or Podman's host network in DinD) so the repo URL's `localhost` works | `127.0.0.1:8080` |
| split, turnkey compose | nspawn shares the worker's namespace, which is on the server's compose network | `aurcache:8080` |
| split, remote worker | over the LAN, like any client | the server's address |

The usual answer -- "with OAuth off the API is open to the LAN anyway" -- is
true and does not settle it: the LAN is people the operator lets in; builds
are code from whoever wrote the PKGBUILD.

And the API is only the instance of this that AURCache owns. The same build
reaches everything else on the LAN: the NAS's admin UI, other apps, printers,
routers -- whatever answers without a password, or with a default one.

## Options

### D. Builds in a network of their own, with no LAN but what they need

Each build runs in its own network namespace, which reaches the internet and,
on the LAN, exactly the endpoints a build legitimately uses. Everything else
private is unreachable -- the API, the worker port, the NAS, the rest of the
LAN -- whatever authentication any of it has.

**What a build needs from the LAN is already known to the worker**, so the
allowlist is derived, not configured:

- **the AURCache repository**: `client.repo_section()`, which the worker
  appends to every build's `pacman.conf` (`aurcache-worker/src/job.rs`);
- **the mirrors**: `job.mirrorlist`, which the server sends with every job. A
  LAN mirror -- a flexo or pacoloco on the same NAS, say -- is in it.

Both are allowed as `ip:port` pairs, not hosts. On a NAS where the mirror, the
repository and the API share one address, `nas:7878` (flexo) and `nas:8081`
(repo) stay open and `nas:8080` (API) and `nas:8083` (worker port) do not.

How the pairs are built:

- **The URL's own port only**: the explicit one, else the scheme's default --
  what the client dials, as `build_firewall.rs`'s `endpoint()` already
  computes it. A mirror host's other ports stay closed, which on a NAS is
  the point: the mirror shares its address with everything else the NAS runs.
  The one thing this costs is a LAN mirror that redirects to another LAN port
  or host; flexo and pacoloco do not, and public mirrors are unaffected
  either way, since only private destinations are filtered.
- **Every address the name resolves to**, each on that port.
- **The repository and the mirrors are one set.** They often share a host --
  the AURCache instance and its mirror on one NAS -- and overlapping entries
  simply merge.

**What `ip:port` cannot separate: services sharing one port.** When a
reverse proxy serves the repository and the API under one address and port --
`https://aurcache.example.com` for both, or an ingress proxy on the NAS
fronting every app on 443 -- allowing the repository allows everything behind
that proxy, the API included. Telling them apart takes a proxy that reads the
requested name (`Host`/SNI), which is a different design. The worker can
detect the common case, the repository on the same address and port as the
worker protocol (as `build_firewall.rs` does), and should say so; beyond
that, the docs should recommend serving the repository (and a LAN mirror) on
a port of its own, and **A** is what covers a deployment that cannot.

What it cannot infer is a LAN host named in a PKGBUILD's own `source=` -- a
private Gitea, typically, and likely for packages that use build credentials.
`WORKER_BUILD_ALLOW=host:port,...` adds those; without it they fail with a
refused connection, which at least says where to look.

#### The chroot worker

- Per build, the worker creates a network namespace and a veth pair, NATs the
  namespace out through its own interface, and starts nspawn with
  `--network-namespace-path=`.
- On the worker's side of the veth, in forwarding: allow the derived and
  configured `ip:port` pairs; reject RFC 1918, `100.64.0.0/10`, link-local,
  `fc00::/7`, multicast and the worker's own addresses -- Docker's
  `172.16.0.0/12` networks, and so the server's compose network, among them;
  allow the rest.
- DNS: a forwarder on the veth gateway, passing queries to the worker's own
  resolver. Docker's (`127.0.0.11`) is the worker's loopback and not reachable
  from another namespace, and public resolvers are not an option: the
  allowlisted endpoints are LAN names -- `truenas`, the compose service
  `aurcache` -- that only the internal resolver knows. The forwarder still
  resolves every LAN name; connecting to them is what is blocked.
- **The forwarder keeps the allowlist current.** Rules match addresses, but
  the allowlist is names, and a LAN host's address can change -- DHCP, most
  often -- during a build that runs for hours. So the allowed set is an nft
  set with per-element timeouts, and when the forwarder answers a query for an
  allowlisted name, it adds each returned address on that name's port, expiring
  after the record's TTL (with a floor, since LAN resolvers often hand out TTL
  0). A changed address is allowed from the build's next lookup of it; one no
  longer returned ages out instead of staying open to whatever device gets it
  next. The address is added *before* the answer is returned, so the
  connection that follows the lookup is never refused for being early.
  dnsmasq's `--nftset` does the same; here the forwarder is ours.
- The set is seeded at build start by resolving the allowlist through the same
  resolver, for anything that connects without a lookup of its own. Every
  address the rules allow is one the build was, or would be, given.
- The rules are by address, so a public name that resolves to a LAN address
  (DNS rebinding) is still blocked unless it lands on an allowed `ip:port`,
  and the forwarder only extends the set for allowlisted names, never for
  whatever a build happens to look up.
- This is also what `build_firewall.rs` lacks: it resolves once, at worker
  startup, and follows no change until a restart.
- IPv4 only, at first: IPv6 is disabled in the build's namespace
  (`net.ipv6.conf.all.disable_ipv6`), so there is no second address family to
  filter, and no IPv6 route by which the LAN could be reached unfiltered.
  Tools that try AAAA first fall back to IPv4 at once, finding no route; the
  cost is sources reachable only over IPv6, which are rare. Filtering IPv6 like
  IPv4 (`fc00::/7`, link-local, the worker's own prefixes, NAT66 or routed
  prefixes out) is wanted later, as its own step, once v4 is in place.
- Requirements: `NET_ADMIN` and per-namespace forwarding, which the worker's
  privileged container already has. Nothing on the host -- which matters on
  TrueNAS, where host firewall rules are not the operator's to manage.

**Stronger than `build_firewall.rs`.** That rule matches the build user's uid,
so a build that reaches root in its chroot (`sudo pacman` with a crafted
package) leaves it behind. Root in the build's own namespace can change the
rules there, but not the ones on the worker's side of the veth. D subsumes the
worker-port rule.

**The catch: the download phase runs outside the chroot.** `makechrootpkg`
runs `makepkg --verifysource` on the worker, as the build user, before
entering the container -- and that is the phase that does most of the
fetching. It has to run in the build's namespace too, or D isolates
everything but the part that talks to the network. That is awkward through
devtools (the nspawn wrapper can add the namespace to the container, but
not to this phase) and natural in the build runner of `build-runner.md`,
which already proposes owning both phases and the nspawn arguments. **D for
the chroot worker belongs in that work**, not before it.

#### The hybrid image

- **DinD mode**: the same with a Podman bridge network instead of Podman's host
  network. Builds reach the server at the bridge's gateway address, so the
  repository URL the builder appends must name the gateway rather than
  `localhost` (the builder writes that section, so it can). One rule set in
  the outer container: from the bridge's subnet, allow the server's repo port
  and the derived pairs, reject the server's other ports and the private
  ranges, allow the rest. IPv4 only at first, as for the chroot worker (a
  Podman network has no IPv6 unless created with it). DNS goes through a
  forwarder on the gateway too,
  for the same reason as for the chroot worker above. No build-runner
  dependency: the legacy builder has no host-side download phase.
- **Host mode**: no clean way. Build containers run on the host's daemon, and
  the worker cannot change the host's firewall. That mode stays as it was
  before workers existed.

### A. Authentication by default

Make "no OAuth" mean "API token", not "open". On first start the server
creates an admin token, stores it under `/app/data`, and prints it to the log;
the UI, finding no OAuth, asks for a token once and keeps it in a session
cookie. The CLI already speaks tokens. `AURCACHE_AUTH=none` keeps today's
open behaviour for those who want it.

- Closes the API for every setup, hybrid host mode included, and for the LAN as
  well -- but only the API: the rest of the LAN stays reachable from builds.
- Upgrades stay edit-free but not click-free: someone has to read one token
  out of the log once, and that lands on exactly the upstream users this
  merge is for.
- Work: a token login page in the frontend, bootstrap and storage, docs.

### B. Firewall builds off the API port only

Superseded by D, which does this and more with the same machinery.

### C. Refuse loopback in the API (hybrid only)

With a flag the hybrid entrypoint sets, the server refuses requests whose peer
is loopback: in that container nothing legitimate reaches 8080 that way. A
few lines; breaks a user's own `curl localhost:8080` healthcheck inside the
container. Superseded by D for DinD mode; still the only cheap cover for host
mode.

## Recommendation

**D**, in two steps:

1. **Hybrid DinD first.** Self-contained -- Podman network, gateway repo URL,
   one rule set in the entrypoint's container -- and it covers the setup
   upstream users land on, where OAuth is most often off.
2. **The chroot worker with the build runner**, since the download phase is
   only isolable once AURCache owns it.

**C** for hybrid host mode, since nothing else reaches it cheaply. **A**
stays worth doing as defence in depth -- it is the only option that also
closes the API to the LAN itself -- but it changes what upgrading means for
OAuth-less users, so it should be decided on its own.

## Open questions

- D's rules change when the mirrorlist does. Per build is the simplest
  correct answer, since each job carries its own mirrorlist.
- A: can the token prompt be skipped for the very first visit
  (claim-on-first-load), or is that the same open door with a timer on it?
