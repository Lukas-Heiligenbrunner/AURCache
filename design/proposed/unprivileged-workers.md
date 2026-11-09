# Running workers without host root

Status: **Proposed** (exploration) · Last updated: 2026-10-01 · TrueNAS facts checked on 25.10

Upstream issue: Lukas-Heiligenbrunner/AURCache#276 ("Running AURCache without
--privileged or binding the docker socket"). The issue is about upstream's
docker-in-docker builder, but the concern carries over directly: every
documented way to run a worker here gives it root on the host.

This doc is for operators who want a guarantee that does **not** depend on
AURCache being right. That means both parts of AURCache that could get it
wrong:

- **its sandboxing of PKGBUILDs**: the chroot, the build user, and the
  Landlock policy of `aurcache-sandbox`;
- **AURCache itself**: a bug in the worker, a compromised dependency, or a
  worker binary that is not what it should be.

The target case is a worker on a TrueNAS box. If either of the above fails,
the damage should stay inside something the operator can throw away, and the
NAS, its pools and its other apps should not be affected.

It surveys the options and recommends an order. Nothing here is implemented
yet.

---

## Where things stand

### What the worker does as root

The worker runs as `aurcache` with `NOPASSWD: ALL`
(`packaging/aurcache-worker.sudoers`). The container images run it the same
way, and the hybrid image runs it as root. Its root actions are:

| what | why | where |
|---|---|---|
| `losetup`, `mkfs.btrfs`, `mount`, `umount` | open the storage pool (image or device) | `aurcache-chroot/src/pool.rs` |
| btrfs subvolume/snapshot/qgroup ioctls | one chroot per build, `WORKER_DISK_MAX` | `pool.rs`, `qgroup.rs` |
| `rm`, `touch` on pool paths | sweep and cleanup | `pool.rs` |
| `makechrootpkg` → `arch-nspawn` → `systemd-nspawn` | run each build in its chroot, as root in it | devtools |
| `iptables`/`ip6tables` | keep builds off the worker-protocol port | `aurcache-worker/src/build_firewall.rs` |
| cgroup writes | one cgroup per build: `memory.peak`, limits, kill | `aurcache-worker/src/cgroup.rs` |

The sudoers file already says what that grant means: "with NOPASSWD:ALL,
code execution in the worker is root". Its narrowing is intentionally left
undone, because `makechrootpkg` takes free-form paths and bind mounts, so any
allowlist that names it grants as much as `ALL`.

### What root means in the container

Every compose file runs the worker `privileged: true`
(`compose/docker-compose.yaml:129`). A privileged container is not a
security boundary against its own root user. It sees **every host device**,
including on TrueNAS every zvol (`/dev/zd*`), and so every VM disk and every
other app's zvol. It can mount host filesystems, load kernel modules, and
write to `/sys`. Root in the worker container is root on the NAS.

The legacy docker builder (`aurcache-worker-docker`, the hybrid image's
socket mode) mounts the Docker socket, which is the same thing by a
different route.

### The paths to host root

Starting from a hostile PKGBUILD:

1. **The host-side source fetch**: `makepkg --verifysource` runs on the
   worker, confined by Landlock with no write access outside its own build.
   A kernel bug or a Landlock gap gets code running as `builder` on the
   worker, which has no sudo.
2. **The build**: it runs in the chroot as `builder`, which devtools lets
   `sudo pacman`. So installing a package it crafted gives root **in the
   chroot**. That is by design, and it is easy.
3. **From root in the chroot to root on the worker**: the chroot is a
   `systemd-nspawn` container **without user namespacing**, so its root is
   the worker's root with fewer capabilities. systemd's documentation does
   not treat that as a security boundary. Escaping it is harder than steps 1
   and 2, but it is the step every other one relies on.
4. **From root on the worker to root on the host**: nothing stands in the
   way, because the container is privileged.

Starting from AURCache itself, the path skips straight to step 4. The worker
is root through `sudo`, and the container adds nothing on top.

So the choice is between **adding boundaries at step 3** (inside the worker)
and **at step 4** (around it). Only step 4 answers "I don't trust AURCache",
because step 3 is AURCache's own code. A third way is to **drop the need for
root** altogether and give up what it pays for (option G).

---

## What counts as contained

A deployment passes if, with arbitrary code running as root **in the
worker**, the host keeps:

- **its storage**: no other zvol, dataset or host path readable or writable,
  beyond what the operator gave the worker (its pool);
- **its kernel**: no module loading, no raw `/sys` or `/proc/sys` writes, no
  host mounts;
- **its other services**: no access to the host's network namespace, the
  Docker socket, or other containers' filesystems.

What the worker is **allowed** to reach, and so what it can still damage:

- its own pool (all of its builds and caches);
- the server's worker protocol, as an approved worker. It can upload
  malicious packages, which is what a compromised worker can always do, and
  why `signed-repository.md` treats worker approval as the real trust
  decision;
- the network, as far as the operator's own network rules allow.

---

## Options

### A. Narrow the privileged container

Replace `privileged: true` with `cap_add: [SYS_ADMIN, NET_ADMIN, MKNOD]`, a
`devices:` entry for the pool zvol only, a writable cgroup mount, and
`seccomp`/`apparmor` set to `unconfined`.

- **Removes the accidental exposure.** The container no longer sees every
  device, which is the most direct risk on a NAS.
- **It is not a boundary.** `CAP_SYS_ADMIN` in the host's user namespace is
  close to root: it can mount, it can `setns`, and with seccomp and AppArmor
  off nothing narrows what it can do. Without a user namespace, root in the
  container is root on the host.
- **It is cheap and fits everywhere.** TrueNAS custom compose takes all of
  these settings.

Worth doing as a default improvement, but it **does not meet the goal**, and
the docs must not present it as if it did.

### B. A user-namespaced container (the issue's approach)

Run the worker container in a user namespace, through rootless
Docker or Podman, or Docker's `userns-remap`. Root inside the container is
then an unprivileged uid range on the host, and its capabilities only apply
to namespaces it owns. That is a real boundary: getting from there to the
host needs a kernel bug, not a missed setting.

What breaks, part by part:

| part | in a user namespace |
|---|---|
| pool: image or device | **refused**: `losetup` needs the host, and btrfs is not mountable from a user namespace. Only `Backing::Mount` remains, with the host mounting the btrfs and passing it in |
| subvolumes and snapshots | expected to work for the owner; deletion needs the `user_subvol_rm_allowed` mount option. *To verify.* |
| qgroups (`WORKER_DISK_MAX`) | **lost**: btrfs quota ioctls check `CAP_SYS_ADMIN` in the host namespace. The total would have to come from the host (a quota on the backing zvol or dataset) and be reported rather than enforced |
| `systemd-nspawn` | needs to create its own mount, PID and IPC namespaces and to mount `/proc` and `/sys`, which a user namespace allows. Whether nspawn accepts running this way is the **main unknown**. *Spike.* |
| per-build cgroups | work with a delegated cgroup v2 subtree (rootless Podman under systemd delegates `memory`, `pids`, `cpu`). Without delegation the worker falls back to what it does today in an unprivileged container: no figure, no limits |
| build firewall | works: `iptables` acts on the container's own network namespace, which its user namespace owns |
| `sudo` in the chroot | needs setuid, so `no-new-privileges` must stay off (the issue hit this). The worker itself could run as container-root and drop `sudo` altogether, since that root is not the host's |
| AppArmor | `docker-default` denies `mount`; needs a custom profile (preferred) or `unconfined`. With a user namespace underneath, `unconfined` loses much less than it does in option A |

The shape of a supported "userns mode":

- `WORKER_POOL` must be an existing mount. The worker refuses image and
  device backings with a message that names this mode, instead of failing
  in `losetup`.
- `WORKER_DISK_MAX` is advisory. The pool reports usage, the worker stops
  claiming work when the pool is full, but no qgroup enforces the limit.
  This needs its own `Backing` variant so that the Workers page can show that
  the limit is not enforced.
- A compose file, `compose/docker-compose.userns.yaml`, for rootless Podman
  and `userns-remap` hosts.
- A `test-kernel.sh`-style suite in a rootless container, which is the only
  way to keep the mode working.

**On TrueNAS specifically**, this route is closed. On 25.10, the
`/etc/docker/daemon.json` written by the apps service (`data-root`
`/mnt/.ix-apps/docker`, cgroupfs driver, overlay2) has no `userns-remap`, and
there is no setting that adds one. There is no Podman, and no rootless
Docker. So option B is for generic Docker and Podman hosts, not for TrueNAS
apps.

### C. An unprivileged system container (Incus/LXC)

TrueNAS 25.10 runs system containers through Incus (the `virt.instance`
API; VMs are a separate libvirt service, see option D), and an Arch image is
among those it offers. **TrueNAS 26 moves containers to libvirt's LXC
driver**, so anything here that depends on an Incus-specific feature has a
short life on TrueNAS. What the kernel allows inside an unprivileged
container carries over; how the middleware configures that container may
not.

#### What a spike on TrueNAS 25.10 found

A throwaway container (TrueNAS's defaults, nothing raw set), probed from a
boot-time script:

| | result |
|---|---|
| id map | container 0-567 and 569 to about 458,000 map to a high host range; plus **ids mapped directly to the same host id**: TrueNAS lets the admin mark accounts for direct mapping and writes them as `raw.idmap`, and by default that is only 568, the `apps` user. TrueNAS's own `userns_idmap` field shows only the first extent, which is misleadingly small |
| capabilities | the full set, namespaced; seccomp filter on; `NoNewPrivs` 0 |
| `security.nesting` | off, and the TrueNAS API has no way to set it (nor any raw config key) |
| cgroups | cgroup v2, `memory` and `pids` delegated, subgroups can be created |
| namespaces | new mount/PID/IPC/UTS namespaces work; a **nested user namespace does not** (`mount /proc` and `gid_map` writes refused) |
| mounts | tmpfs, proc and overlay mount; no loop devices |
| `iptables` | works on the container's own network namespace |
| `mkarchroot` | fails: it mounts a devtmpfs `/dev` |
| `systemd-nspawn` | fails: cannot mount `/proc` in the payload; idmapped mounts and `mknod` are refused, so `--private-users` in either ownership mode fails too |
| rootful Podman | works once `default_sysctls = []` (it cannot write `net.ipv4.ping_group_range`), with native overlay on the container's ZFS root. A full `makepkg` build, `sudo` inside, and a `--memory` limit all work. `--userns=auto` fails, as nested user namespaces do |
| a zvol as a container disk | **refused by the middleware**: "ZVOL are not allowed for containers". A container disk must be a dataset mountpoint, bind-mounted in |
| an Incus-formatted btrfs volume | not reachable: `virt.volume.create` makes only `BLOCK` volumes, with no config keys |

Two findings reshape this option:

- **The chroot executor does not run here.** `systemd-nspawn` is what
  `makechrootpkg` builds in, and it cannot start its payload without
  nesting. Nesting is exactly what the middleware does not expose. So on
  TrueNAS 25.10, option C is only reachable combined with option H
  (Podman builds), not with the chroot executor.
- **Directly mapped ids are host ids.** Anything in the container running
  as a directly mapped id acts on the host as that account. By default that
  means 568, the `apps` user, which owns the app datasets. The set is the
  admin's to configure, and the rule holds for all of them: the worker, its
  build users and any build container must not run as a directly mapped
  id, and the docs must say not to add a mapping for the worker's own ids.
  The worker can check this itself: `/proc/self/uid_map` shows an identity
  extent (inside id = host id) outside the initial namespace, and the worker
  can refuse to run itself or a build as such an id.

The id map itself is not a constraint: about 458,000 ids is room for any
layout. What is missing is nesting, not ids.

**The packages are unaffected by the map.** A package records the ids its
build *sees*, and `package()` runs under `fakeroot`, whose `chown` never
touches the real filesystem. The spike's test package, built by `makepkg` in
a Podman container inside the mapped container, lists its files as 0, 33
(`http`) and 65534 (`nobody`) under `bsdtar --numeric-owner`, exactly as a
build on a stock host would. The host range behind the map never appears.
Only a `package()` that chowns to a *dynamically allocated* system user
records a machine-specific id, and that is broken on any builder. Arch's
guidelines say to use `sysusers.d`/`tmpfiles.d` instead, and the server
could flag such files at publish time on any worker
(`package-suspicion-signals.md`).

#### The pool

The chroot executor cannot run, and the pool exists to serve it, so the pool
question matters less. For completeness:

- **btrfs**: the kernel has it, but a container cannot mount it, and on
  TrueNAS 25.10 neither route that would hand it a mounted btrfs is open
  (no zvol on containers, no Incus-formatted volume through the API).
  Doing either as root with the `incus` CLI, beside the middleware, is
  possible and not something to document for users, all the more with
  Incus leaving TrueNAS.
- **The container mounting the device itself**, through Incus's `mount`
  interception, is ruled out on principle: the host kernel would then parse
  a filesystem image the container controls byte by byte.
- **ZFS instead of btrfs**: see below.

#### A ZFS pool

The kernel is ZFS-enabled (OpenZFS 2.3 on TrueNAS 25.10), and OpenZFS 2.2
added **delegating a dataset to a user namespace** (`zfs zone`). The
namespace can then create child datasets, snapshot, clone, set properties
and mount within that dataset, and nowhere else. Incus exposes this as
`zfs.delegate=true` on a volume. That gives a pool with everything the
btrfs one has, and something better in one place:

| btrfs pool | ZFS pool |
|---|---|
| base chroot: a subvolume | a dataset, with a snapshot per refresh |
| a build's chroot: a snapshot | a clone of the base's snapshot: instant, copy-on-write |
| `WORKER_DISK_MAX`: simple quotas, whose accounting has had bugs | `quota` on the pool dataset, `refquota` per build: ZFS's own quotas, enforced exactly |
| usage: qgroup numbers | `used` / `referenced` properties |

The costs:

- **A second pool implementation.** `aurcache-chroot/src/pool.rs` is btrfs
  through and through (subvolumes, qgroups, `mkfs`). A ZFS backend means
  a `Pool` trait with two implementations, and `test-kernel.sh` coverage for
  both. It would also serve **native Arch-on-ZFS hosts**, which today have
  to give the worker a btrfs image or device.
- **Userspace has to match the kernel module.** `zfs-utils` in the Arch
  userspace (from archzfs, not the official repositories) talks ioctls to
  the host's module, so its version has to stay compatible with whatever
  the host ships. That is a moving dependency the worker image would have
  to track.
- **`/dev/zfs` in the container.** Delegation exposes the ZFS ioctl
  interface of the host kernel to the container. The zone checks confine it
  to the delegated dataset, but it is more host-kernel surface than a plain
  bind mount. It is much less than a mounted image the container wrote, but
  not nothing.
- **It still does not bring nspawn back.** Without nesting, a ZFS pool on
  TrueNAS would feed the same chroot executor that cannot start. So on
  TrueNAS a ZFS pool only makes sense with Podman builds (option H), as its
  image and container store (Podman has a ZFS storage driver), which is a
  much smaller win.
- **TrueNAS does not expose delegation** (the middleware sets no
  `zfs.delegate`), and TrueNAS 26's libvirt LXC has no equivalent that
  its middleware is known to offer.

So a ZFS pool is worth a design of its own **for native ZFS hosts and for
container hosts that allow nesting**. It is not a way to make option C work
on TrueNAS.

### D. A virtual machine

Run the worker in a VM: on TrueNAS, through the libvirt `vm` service (the one
behind TrueNAS's Virtual Machines page, with KVM available), or on any
hypervisor. Inside it, the worker runs exactly as it does now, as the native package or the
worker image, privileged or not. It is just a remote worker
(`design/implemented/remote-workers.md`): pull-only, over mTLS, needing
nothing from the host but a network route to the server's worker port.

- **The strongest boundary on offer.** A worker compromise, whether through
  a PKGBUILD or through AURCache, ends at the VM's root. Reaching the host
  takes a hypervisor escape.
- **Nothing is lost.** The pool is a virtual disk (on TrueNAS a zvol, which
  a pool can already be given as `Backing::Device`), and the worker opens it
  with qgroups, nspawn, cgroups and the firewall all
  unchanged. Every current test applies.
- **Costs**: memory is reserved for the VM rather than shared with the host's
  ARC, there is a little overhead on virtio I/O, and there is a second OS to
  keep updated. For a build worker, which is idle or saturating the machine,
  the memory reservation is the one that matters.
- **No code change.** It needs documentation: a TrueNAS VM walkthrough, and
  `aurcache-cli setup worker` output for a fresh Arch VM.

### E. A VM-backed container runtime (Kata)

Kata Containers runs each container in a lightweight VM, and supports
`privileged` inside that VM. The existing compose file would work with
`runtime: kata` and get option D's boundary. It is not available on TrueNAS
and needs host setup, so it is mentioned for generic hosts, not proposed as
something to support.

### F. Reinforcing step 3 inside the worker

This is not an alternative to the options above: it helps every deployment,
including native ones, and it is the part only this codebase can do.

- **User-namespaced builds.** Run each build's `systemd-nspawn` with
  `--private-users=pick --private-users-ownership=map`, so that root in the
  chroot is an unused uid range and not the worker's root. Step 2 (root in
  the chroot through `sudo pacman`) then gains nothing on the worker. The
  images already rewrite nspawn's arguments in `docker/nspawn-wrapper.sh`;
  the native package would need the same. *To verify*: idmapped mounts on
  the pool's btrfs (supported by the kernel since 5.15), devtools' bind
  mounts (`-d`, the package cache, `SRCDEST`) under an idmap, and
  `makechrootpkg`'s `chown`s of the build directory.
- **A root helper instead of `sudo ALL`.** The sudoers comment already says
  what real narrowing takes: removing free-form arguments from the root
  boundary. A small `aurcache-worker-helper`, run as root with a fixed set
  of verbs that take a job id or a pool-relative name and never a path
  (`open-pool`, `snapshot-root`, `run-build <job>`, `delete-subvolume
  <name>`, `install-firewall <uid> <port>`), would make a compromised worker
  process exactly as powerful as those verbs. Together with user-namespaced
  builds, `run-build` would no longer amount to root either. This is the
  larger change: devtools' scripts would have to run inside the helper, not
  be called with arguments the worker chooses. `design/proposed/build-runner.md`
  goes further and replaces `makechrootpkg` and `arch-nspawn` with a runner of
  our own, because `arch-nspawn` decides binds from the chroot's
  `pacman.conf`, which no wrapper can make safe.

Option F is what allows trusting AURCache a little **less** on any
deployment. Options B to E are what let an operator skip trusting it.

### G. Do without the privileged parts

Options B to E keep the worker's current design and find a place where its
root is harmless. The other way round is a worker that never asks for
privilege at all: an **ordinary container**, with `cap_drop: [ALL]`, the
default seccomp and AppArmor profiles, `no-new-privileges`, no devices, and
no socket. That is the same boundary TrueNAS gives every other app, and a
PKGBUILD or a worker bug that gets out of it needs a kernel bug. The server
already runs this way.

The worker runs as root **in that container**, which is not the host's
root, so it has no need for `sudo`. The container itself becomes the build
environment, and builds run in it directly rather than in a chroot inside
it. This is close to the docker builder's model (`aurcache-worker-docker`),
minus the socket. Its build script (`commands.rs`: keys, dependencies,
`makepkg`) is written for a fresh Arch container, so a third executor beside
`ChrootExecutor` and the docker one (`KIND = "local"`, say) could run that
script in place.

What that gives up, part by part:

| today | without privilege | how much it costs |
|---|---|---|
| a fresh chroot per build, snapshotted from the base | builds share the container's root. Dependencies are installed into it and removed afterwards (`makepkg -r`), and the container is recreated to reset it | **the main loss.** A package that forgets a `makedepends` can still build because an earlier build left the dependency behind, and one build's `post_install` scriptlet can leave state behind for the next. Builds are less hermetic, and "it builds on AURCache" means less |
| several builds at once | **one** at a time: two builds would install conflicting dependencies into the same root | throughput. It is fine for a homelab with tens of packages, and not for a fleet |
| the build runs as `builder` in a chroot, the worker as `aurcache` outside | both in one container; the build as `builder`, the worker as root | the build can read whatever the container holds, including the worker's mTLS key. `aurcache-sandbox --read-except <identity dir>` around the whole build closes that; it already exists for the server's parse |
| the build firewall (`iptables`, needs `NET_ADMIN`) | Landlock's network rules (`--no-net` already uses them): deny TCP connect to the worker-protocol port | about the same. Landlock's rules are by port, and the firewall's are by uid and port, so this has the same limitation when the worker port is 443 |
| the btrfs pool: snapshots, qgroups, `WORKER_DISK_MAX` | a plain directory | no per-build limit. The total comes from the host (a ZFS dataset quota on the volume, which TrueNAS sets natively) and the worker reports usage rather than enforcing it |
| per-build cgroups: `memory.peak`, per-build limits, `cgroup.kill` | `/sys/fs/cgroup` is read-only, which the worker already copes with | no memory figure and no per-build limits. The container's `mem_limit` is the total, and a process-group kill replaces `cgroup.kill`. With one build at a time, the container *is* the build's cgroup, so `docker stats` still tells the story |
| base chroot refresh (`root.next` / `root.prev`) | `pacman -Syu` of the container, between builds | no rollback point. A bad upgrade breaks builds until the image is pulled again, which is the recovery path anyway |
| kept failed builds for `arch-nspawn` | the kept build directory only | debugging a failure means rebuilding it by hand |
| foreign architectures | unchanged: an emulated container (`platform: linux/arm64` with binfmt registered on the host) needs no privilege of its own | nothing |

Resetting the root is the awkward part. A container cannot recreate
itself, so either the worker keeps a list of what the image shipped and
removes everything else between builds (the removal is `pacman -Rns`,
possibly with scriptlets still left behind), or the worker exits after each
build and something outside recreates it. Compose's `restart` reuses the
container, and TrueNAS has no "recreate on exit". So the first cut is the
list-and-remove approach, with a periodic hint to the operator to redeploy.

**Who this suits**: an operator with a small package set, one worker, and
more concern for the host than for build hermeticity. That is plausibly the
person on the upstream issue, and it is the only option here that fits
TrueNAS apps as they are. It is also the smallest change for its users:
one compose service, nothing to pass through, and no VM.

**What it costs the project**: a third executor to keep working. Some of
its differences show up in the product: the Workers page has to say that a
worker's builds are not isolated from each other and that its disk limit is
advisory, and routing may want to keep packages that are known to leave
state behind away from it. It should be reported as its own `KIND`, so the
difference stays visible and never looks like a degraded chroot worker.

---

### H. A container per build, from inside the worker (podman or DinD)

The approach on the upstream issue: the worker's container runs its own
container engine (rootful Podman, or `dockerd` for DinD), and each build is
a fresh container from an Arch image. This is the docker builder's model
(`aurcache-worker-docker`, the hybrid image's legacy mode) with the engine
moved inside the worker instead of reached through the host's socket.

Functionally it works, and it keeps what option G gives up: every build
starts from a clean image, builds run concurrently, and each build is its
own cgroup. It does not need btrfs either. Storage is the engine's native
overlay on a volume that is not itself overlay; on TrueNAS that would be a
dataset, which needs OpenZFS's overlayfs support (2.2 and later). The
TrueNAS 25.10 spike confirmed this works. The disk limit is that dataset's quota.

Whether it contains anything depends on what surrounds the worker's
container, because the engine inside needs `CAP_SYS_ADMIN`, AppArmor
relaxed and `no-new-privileges` off (the issue lists each):

- **In a plain Docker container, as TrueNAS apps are**, a *rootful* engine
  is option A: `CAP_SYS_ADMIN` in the host's user namespace, with the
  protections that would narrow it switched off. It is still an improvement
  on the socket (no host engine to drive, no host devices), but it is not a
  boundary. A *rootless* Podman needs none of that: no added capability and
  no `--privileged`, only a device, a seccomp profile, AppArmor relaxed and
  writable cgroups. Its builds sit behind a user namespace of their own. See
  "Rootless Podman in a plain Docker container" below, tested on TrueNAS.
- **Under `userns-remap` or rootless Docker/Podman**, the issue's own setup,
  it is option B's boundary. The engine is known to work there, which is
  exactly what option B lacks for nspawn.
- **In an unprivileged system container**, it is option C's boundary. The
  TrueNAS 25.10 spike above ran this for real: without nesting, rootful
  Podman builds a package end to end, with `sudo`, native overlay storage
  on ZFS, and per-build memory limits. Every build container shares the
  system container's user namespace (nested ones are refused), so builds are
  kept apart by Podman's namespaces and cgroups but not by ids, and a build
  that escapes its Podman container is root in the worker's container. The
  host is still behind the user namespace.

Compared with the chroot executor, H trades the btrfs pool (snapshots,
qgroups, kept trees you can `arch-nspawn` into) for an engine that works
where nspawn does not. On TrueNAS 25.10, **C+H** (a system container running
the worker with Podman builds) is the only container-shaped alternative to
option D's VM, and the spike shows it works. TrueNAS 26 moving containers to
libvirt LXC means redoing the spike there before documenting it.

#### Rootless Podman in a plain Docker container

Tested 2026-10-01 on an Arch host (Docker 29.8 with the containerd
snapshotter, Podman 6.1, ext4, kernel 7.2) and on the TrueNAS builder
(Docker 28.3.1, overlay2 on ZFS, AppArmor active, kernel 6.12). The outer
container is `quay.io/podman/stable`, the engine runs as its `podman` user
(uid 1000), and each build is `podman run` of the Arch image. Scripts were
throwaway; the flags are what matters.

| outer container | result |
|---|---|
| `docker:dind`, unprivileged | fails: `mount: permission denied` |
| `docker:dind`, `--privileged` | works, and is option A |
| `docker:dind-rootless`, unprivileged | works with seccomp, AppArmor and systempaths unconfined, `/dev/fuse` and `/dev/net/tun` |
| rootless Podman, default seccomp, with or without `/dev/fuse` | fails: `cannot clone: Operation not permitted` |
| rootless Podman, `/dev/fuse` + `seccomp=unconfined` | **works**: base-devel installs and `makepkg` builds, 13 s for a trivial package |
| the same with Podman's own `seccomp.json` as the outer profile | **works**, so the outer container does not need `unconfined` |
| the same on TrueNAS, default AppArmor | fails: overlay storage cannot make its mount private |
| the same on TrueNAS, `apparmor=unconfined` | **works**, build included |

Rootless DinD reaches the same place with more relaxed (a tun device,
systempaths), so Podman is the engine to use.

**Limits.** Unprivileged Docker mounts `/sys/fs/cgroup` read-only, and a
`--memory` given to the inner Podman is then dropped without a word
(`memory.max` stayed `max`, the peak went past it). With
`--security-opt writable-cgroups=true`, which TrueNAS's 28.3.1 accepts, the
container's root does the delegation `cgroup.rs` already does (move into a
leaf, enable `memory pids cpu`, `chown` a subtree to `podman`), and Podman
runs with `--cgroup-manager cgroupfs --cgroups=enabled
--cgroup-parent=/<subtree>`. Without `--cgroups=enabled`, rootless Podman on
cgroupfs creates no cgroups and ignores limits silently. With it, a 100 MB
build is OOM-killed (137) and a 1 GB one survives, on both hosts; on Arch
`memory.max`, `memory.peak` and `oom_kill` read correctly from
`<subtree>/libpod-<id>`. On TrueNAS that path held nothing, so where the
cgroup lands there is still to find before peak memory can be reported.

**Seccomp.** The outer profile only covers the worker and Podman: filters
stack, and every build container runs under one of its own (`Seccomp: 2`
inside in every run). Podman's default build profile still lets a build
`unshare -Ur`, and with a user namespace create a network namespace and
configure it, which is the nf_tables/fsconfig class of kernel attack surface
Docker's default profile exists to close. Running the build with Docker's
default profile (`--security-opt seccomp=<moby default.json>`) blocks
`unshare -Ur` and still builds. So: Podman's profile outside, Docker's
inside. Whether any AUR package needs a user namespace mid-build (tools that
sandbox themselves with bubblewrap) is untested.

**Disk.** `--storage-opt size=` is no use here. Podman refuses it on any
backing but XFS. Docker 29 with the containerd snapshotter *accepts*
`size=200M` on ext4 and ignores it: 400 MB were written without an error,
which is worse than refusing. The bound has to come from outside: the
dataset quota mentioned above, or the worker's btrfs pool, where
`podman run --rootfs <snapshot>` on a snapshot with a 300 MB qgroup limit
stopped `dd` at exactly 300 MB with the base untouched, a 0.27 s snapshot
and a 0.11 s container start. Mounting the pool needed `--privileged`, so
that combination is for hosts where the worker can have a pool, not for
TrueNAS apps.

**Credentials.** Binding the ssh-agent socket's directory into the build
works when the build's uid *is* the agent's uid: container root, or
`--userns=keep-id:uid=1000,gid=1000 --user 1000`. A build uid that maps to a
subuid is refused by `ssh-agent`'s own peer check ("communication with agent
failed"), even with the socket world-writable. So the worker picks the build
uid and maps it to its own, rather than taking the image's.

**Ids: the open problem.** The outer container has no user namespace, so its
ids are host ids. The image's `/etc/subuid` gives `podman` the range
`1:999,1001:64535`, and the Podman namespace maps container root to outer
1000 and container 1-999 to outer 1-999. A build that escapes its container
therefore lands on a **real host account**: uid 1000 is a person's account on
the Arch host, and 1-999 are system users on any host (568 is `apps` on
TrueNAS). This is the direct-mapping rule from option C again. The likely fix
is an outer `podman` user and a subuid range far above anything allocated
(say uid 2000000000 and subuids from 2000100000), plus the
`/proc/self/uid_map` check refusing identity extents. Not yet tested.

So in a TrueNAS app, with the flag set

```yaml
devices: [/dev/fuse]
security_opt:
  - seccomp=./containers-seccomp.json   # Podman's profile, shipped with the image
  - apparmor=unconfined
  - writable-cgroups=true
```

the boundary is a user namespace plus a seccomp filter at least as strict as
a stock Docker container's, with no added capability. That is option B's
boundary without needing `userns-remap` on the host. It still needs, in
order: the id range fixed, the combined flag set rerun on TrueNAS (the
Podman-profile-outside row was only run on Arch), the cgroup path found
there, and an AppArmor profile in place of `unconfined`.

#### Which worker: the docker worker, pointed at Podman

No new worker is needed. `aurcache-worker-docker` speaks the Docker Engine
API through bollard (`Docker::connect_with_unix_defaults`), and Podman
serves a compatible API (`podman.socket`, with `podman-docker` providing
`/var/run/docker.sock`). Its connection error already points Podman users
there. In a system container the setup is:

- Podman rootful, its API socket enabled, and `default_sysctls = []` in
  `containers.conf` (the spike's one needed change);
- the docker worker as a native service beside it, not in a container of
  its own. The network plan then falls to `Default`, because there is no
  `HOSTNAME` container to share, so builds get Podman's bridge and **not**
  the worker's loopback. That is better than the hybrid image's default,
  as long as `AURCACHE_PUBLIC_URL` is not a loopback address.
  `AURCACHE_BUILDER_NETWORK=host` is the escape hatch.

*To verify*: the spike used Podman's CLI, not its API. bollard's attach,
wait and kill against Podman's compat layer (in particular how a non-zero
exit is reported, which the executor matches as `DockerContainerWaitError`)
is the part to test first.

**Keeping a separate Podman executor** would only be worth it for what the
compat API cannot express, and there is little of that: Podman's own
features that matter here (`--userns=auto` per build) are refused in this
setting anyway. One executor for both engines, renamed from "docker" to
"container" so the name stops implying a socket on the host, is the
proposal.

It stays a **best-effort** worker: parity with the chroot executor is not
a requirement, and nothing here blocks the route. Two things are required,
because the containment depends on them: the direct-mapping check above,
and the Podman setup being documented.

The reports it lacks are still **wanted wherever they can be had**. Today
`peak_memory_bytes`, `disk_usage` and `vcs_commits` are always empty and
nothing is kept. Each of them looks obtainable from what the engine
already exposes:

- **peak memory**: the build container's own cgroup has `memory.peak`, and
  the spike showed per-container cgroups with the `memory` controller
  delegated. Reading it just before the container is removed gives the same
  figure the chroot executor reports. The engine's stats endpoint is the
  fallback where the cgroup directory is not visible;
- **disk usage**: the container's writable layer (`size_rw` from inspect)
  plus the output bind mount, which holds the build tree and the sources;
- **VCS commits**: the sources are checked out under the output mount
  (`CONTAINER_SRC`), so the same scan the chroot path runs over its build
  directory applies after the build;
- **kept failed builds**: keep the output directory rather than deleting
  it, and optionally `commit` the stopped container to an image, so a
  failure can be reopened with `podman run`. That is the container
  equivalent of `arch-nspawn` into a kept tree.

Each one that fails to read is reported as `None` and shows as a dash, as
for any value nobody recorded, rather than failing the build.

It is also option G's third executor, with an engine behind it instead of
building in place. The docker executor's build script is written for that
already, so the two would share it.

## The server

The server also runs PKGBUILD code (the parse, under `aurcache-sandbox
--no-net --isolate-ipc`) and holds the database, the repository, the worker
CA key and, once `signed-repository.md` lands, the signing key. It already
runs **unprivileged**. Hardening its compose service costs almost nothing:
`cap_drop: [ALL]`, `security_opt: [no-new-privileges:true]`, a read-only
root filesystem with the data volume writable, and a user namespace where
the host offers one. None of it depends on anything above.

The **hybrid image** puts the server and a privileged worker in one
container, so a build escape there reaches the server's keys and the host
at once. It should be documented as the convenient, trusting setup and not
recommended to anyone reading this doc.

---

## Proposal

1. **Document option D for TrueNAS now.** A TrueNAS (libvirt) VM running the native
   worker, its pool on a zvol passed through as a disk, enrolled as a remote
   worker. It meets the goal in full, needs no code, and keeps the full
   feature set.
2. **Implement user-namespaced builds** (F, first half). This helps everyone,
   and it moves the easy step 2 (root in the chroot) off the worker's root.
3. **Harden the default compose files**: the server settings above, and
   option A's device narrowing for the worker, commented as a reduction of
   exposure and not a boundary.
4. **H on TrueNAS**: the 25.10 spike shows Podman builds work in an
   unprivileged container and the chroot executor does not. Rootless Podman
   also builds inside a plain, unprivileged TrueNAS app (2026-10-01), which
   needs no system container at all. Next is fixing that setup's id range,
   then pointing the docker executor at its Podman. Redo the C+H spike on
   TrueNAS 26's libvirt LXC only if the app route falls through.
5. **Prototype option G**, the unprivileged "local" executor, since it
   is the only one that fits TrueNAS apps unchanged. It needs the
   reset-between-builds approach settled first, and a decision on whether one
   build at a time is acceptable.
6. **Spike option B, and C with nesting**, on a generic host: does nspawn
   run once nesting is on, do snapshots and deletion work on a passed-in
   btrfs, what happens to cgroups. Build userns mode only if the spike passes, with the pool
   limitation stated up front.
7. **The root helper** (F, second half) after that, as its own design.

---

## Open questions

- Does TrueNAS 26's libvirt LXC give containers nesting, a delegated ZFS
  dataset, or a block device? Any one of these changes what option C can
  do there.
- Is a ZFS pool backend (see option C) worth its own design for native
  Arch-on-ZFS hosts, independently of TrueNAS?
- Is a quota the worker cannot enforce acceptable for userns mode, or should
  that mode refuse `WORKER_DISK_MAX` and rely on the size of the backing
  device?
- Is it worth keeping a non-btrfs pool (a plain directory, with full copies
  or overlay chroots) for hosts that can only give the worker a directory?
  It was removed for good reasons; this would be the case for bringing it
  back.
- Should the Workers page show each worker's isolation (privileged
  container, userns, VM, native) from its heartbeat? That would let an
  operator decide what to approve, or route suspicious packages
  (`package-suspicion-signals.md`) to the most isolated worker.
