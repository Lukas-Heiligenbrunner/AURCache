# Replacing makechrootpkg with a build runner of our own

Plan for building packages without devtools' `makechrootpkg` and
`arch-nspawn`: the worker runs each build in `systemd-nspawn` itself, with
arguments it fixes, from inputs that are data rather than paths. It is the
build half of the root helper in `unprivileged-workers.md` (option F), and
the prerequisite for a sudoers grant narrower than `ALL`.

Status: **Proposed** · Last updated: 2026-10-01

---

## Why

### What devtools is for

`makechrootpkg` exists to make **clean** builds: dependencies resolved
against a pristine chroot, nothing leaking in from the packager's machine. It
also fences a build off from the host *in practice*: an unprivileged user, a
separate root, a container. It is not built as a security boundary, and it
does not claim to be one, in either direction:

- **Against the PKGBUILD**, it sources it twice on the host, outside the
  container (`makepkg --verifysource`, and `source PKGBUILD` for
  `pkgbase`). The build user can `sudo pacman` in the chroot, so root there
  is easy, and that root is the host's root with fewer capabilities, because
  `arch-nspawn` does not user-namespace the container.
- **Against its caller**, it offers nothing, by design: the caller is a
  packager on their own machine. `-d` binds any host path into a root
  container, `-r` picks any chroot, `-I` installs any package. The sudoers
  comment says it plainly: an allowlist naming it grants as much as `ALL`.

AURCache's caller is the worker process. A compromised worker (a bug, a
dependency, a forged server response) is root through `makechrootpkg`, and
the sudoers file cannot change that while `makechrootpkg` is what it grants.

### What we have done to it so far

`packaging/patch-makechrootpkg.py` applies ten patches to a copy of the
script, re-derived on every devtools upgrade by a pacman hook:

| patch | why |
|---|---|
| Landlock around `makepkg --verifysource` | the PKGBUILD runs on the host there |
| `TMPDIR` into the job's directory | Landlock refuses `/tmp`, which broke signed sources |
| clear a poisoned source cache, retry once | a fetch killed mid-way wedges VCS sources forever |
| per-build makepkg drop-in into the copy | a drop-in in the base raced between concurrent builds |
| `AURCACHE_*` across `check_root`'s re-exec | sudo drops the rest |
| Landlock around `source PKGBUILD` | the PKGBUILD runs on the host there too |
| `arch-nspawn` wrapper: `--keep-unit`, `--restrict-address-families=` | the container escaped the build's cgroup; a systemd default is changing |
| skip non-regular files in `pkgdest`, `srcpkgdest`, `logdest`; `chown -h` (three patches) | a build could make host root `chown` any host file, see below |

Each patch must apply exactly once, or the image build fails. That is the
right failure, and it makes every devtools release a possible broken
release here. The worker meanwhile already does, itself, the parts devtools
is best at: the chroot copy is a pool snapshot, deletion and keeping a failed
build are the worker's, and so are the quota and the cgroup. The worker
calls `makechrootpkg -r -l -U -u -d` and none of `-c -T -I -n -C -x`.

### `arch-nspawn` decides binds from the chroot's own `pacman.conf`

This is where wrapping stops working. `arch-nspawn`:

- imports the **host's** pacman keyring into the chroot and writes the
  **host's** mirrorlist over the chroot's;
- binds the first `CacheDir` of the **chroot's** `pacman.conf` read-write,
  and every other one read-only;
- binds read-only every host directory named by a `file://` `Server` in that
  `pacman.conf`.

The worker writes that `pacman.conf` (`chroot.rs` `with_cache_dirs` adds the
per-job and shared caches as `CacheDir`s, and relies on exactly this to bind
them). So whatever can write the chroot's `pacman.conf` can put
`CacheDir = /etc` in it and get the host's `/etc` read-write in a root
container. No wrapper around `arch-nspawn` fixes that, because the decision
is made inside it, from a file.

### Fixed arguments are not enough: two tested routes to root

Tested 2026-10-01 against stock devtools 1.5.1, in a throwaway privileged
Arch container, with `makechrootpkg -r /chroot -l job -U builder` run
through `sudo` by an unprivileged `worker` user: fixed arguments, as a
locked-down sudoers rule would allow.

**A PKGBUILD can have host root `chown` any host file.** `move_products`
and `move_logfiles` run, as root on the host, `chown "$src_owner"` on every
entry of the copy's `pkgdest/`, `srcpkgdest/` and `logdest/`. The build owns
those directories, and `chown` follows symlinks. A `package()` that ran
`ln -s /root/canary-pkg /pkgdest/zz-evil-pkg.pkg.tar.zst` (and the same into
`/logdest`), as the unprivileged build user inside the container, left two
root-owned 0600 files owned by `worker`:

```
before: root:root 600 /root/canary-pkg root:root 600 /root/canary-log
after:  worker:root 600 /root/canary-pkg worker:root 600 /root/canary-log
```

`move_logfiles` runs on a failed build as well. For AURCache `src_owner` is
`aurcache`, so any PKGBUILD could give any host file to the worker's user,
and strip the setuid bit of whatever it points at (`chown` clears it). It
needs no worker compromise. The worker refuses a symlinked artifact
(`aurcache-worker-core/src/artifacts.rs`), so nothing was exfiltrated that
way, and no route from `builder` to root through it was found here; upstream,
where the build user is the packager and the PKGBUILD also runs on the host
as the packager, a `/etc/shadow` the packager now owns is one edit from root.
Patched in `patch-makechrootpkg.py` (2026-10-02); the same test then leaves
both files `root:root`, logs `Skipping zz-evil-pkg.pkg.tar.zst: not a regular
file`, and still collects the real package and its log. To report to Arch
privately.

**The caller's own `~/.makepkg.conf` runs as root.** Once root,
`makechrootpkg` sets `HOME` to `SUDO_USER`'s home and calls
`load_makepkg_config`, which sources `~/.config/pacman/makepkg.conf` or
`~/.makepkg.conf` from it. A `.makepkg.conf` running `id -u` wrote `0`.
`aurcache`'s home is `/var/lib/aurcache`, created and owned by `aurcache`
wherever the server package is installed, so a compromised worker writes one
file and the next fixed-argument `makechrootpkg` is root. Not patched: it is
a worker-compromise route, and the worker is root through sudo already. It
is why the sudoers grant cannot be narrowed while it names `makechrootpkg`.

Inputs that reach root without being arguments, then: the caller's home,
`$PWD` (bound read-write as `/startdir`), the preserved environment, the
chroot's `pacman.conf`, and the build's own outputs. The runner takes none
of them from the worker or the build.

---

## What makechrootpkg does for us today

| step | today | in the runner |
|---|---|---|
| copy the base | the worker's pool snapshot (`chroots.rs`); `makechrootpkg` finds it | unchanged |
| refresh the copy (`-u`) | `arch-nspawn copy pacman -Syuu` | the same command, in the runner's nspawn |
| drop-in, `pacman.conf`, mirrorlist | the worker stages files, the patched script `install`s the drop-in as root | the runner writes them into the copy, from data |
| download sources | **host**, as `builder`, Landlocked, `GNUPGHOME` replica, agent socket | **inside the container**, as `builduser` |
| poisoned cache retry | patched into `download_sources` | the in-container script, same rule |
| `pkgbase`/`pkgname` | `source PKGBUILD` on the host, Landlocked | from the job (the server parsed it already) |
| `prepare_chroot` | `builduser` in passwd/group/shadow, `/build /startdir /pkgdest /srcdest /logdest`, `makepkg.conf` lines, `sudoers.d/builduser-pacman`, `safe.directory`, `/chrootbuild` | the runner, as root, beneath the copy only |
| run the build | `arch-nspawn copy /chrootbuild`, binds from `-d` and from `pacman.conf` | one nspawn call with a fixed argument list |
| collect | `chown` + `mv` as root out of `pkgdest`, `logdest` | open beneath the copy, regular files only, copied out |
| delete or keep | the worker | unchanged |

`mkarchroot` stays for now. It builds the base from the worker's own
configuration, never from a job, so its inputs are far less exposed. It is
about 100 lines and can follow later.

---

## Design

### Where it lives

A `build` module in `aurcache-chroot`, beside the pool it already owns. The
crate is what the root helper will be built from, so the runner is written
from the start to need nothing from the worker but a `BuildSpec`.

During the transition (steps 1-3 below) the worker calls it as it calls the
pool today: each privileged action through `cmd::privileged`, so through
sudo. From step 4 it runs inside the helper, and the worker never names a
path to anything running as root.

### Inputs are data, not paths

```rust
pub struct BuildSpec {
    pub job: JobId,               // names the snapshot: job-<id>, job-<id>.data
    pub pkgbase: PkgBase,         // validated: [a-z0-9@._+-], no leading dot or dash
    pub build_user: BuildUser,    // uid and gid, resolved once at startup
    pub makepkg_flags: Vec<MakepkgFlag>,
    pub makepkg_dropin: String,   // contents, not a file
    pub pacman_conf: String,      // contents; checked, see below
    pub mirrorlist: Option<String>,
    pub mounts: Vec<Mount>,
}

pub enum Mount {
    SourceCache,      // cache/srcdest/<pkgbase>  -> /srcdest
    BuildTree,        // cache/builddir/<arch>/<pkgbase> -> /build
    PackageCache,     // the job's own pacman cache -> /var/cache/pacman/pkg
    SharedPackageCache, // read-only
    Keyring,          // the job's GNUPGHOME replica, read-only
    AgentSocket,      // the ssh-agent's directory
}
```

Every host path is derived from the job id, the pkgbase and the pool's own
layout. There is no `Mount::Path(PathBuf)`. A `makepkg` flag is an enum of
the flags the server can send, so `--` followed by anything is not
expressible.

`pacman_conf` is the one input that is still a file's contents: the server
sends it, and repositories legitimately vary. The runner checks it before
writing it: no `CacheDir`, `DBPath`, `RootDir`, `GPGDir`, `HookDir` or
`LogFile` options (the runner sets those itself), and no `file://` servers.
Since nothing reads it to decide binds any more, this check protects the
chroot's pacman, not the host.

### Writing into the copy

Every file the runner writes into the copy, as root, is opened with
`openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)`
from a descriptor on the copy's root, with `O_NOFOLLOW` on the final
component. That covers `etc/pacman.conf`, `etc/pacman.d/mirrorlist`,
`etc/makepkg.conf.d/aurcache.conf`, the `builduser` lines, the sudoers
drop-in, `etc/gitconfig`, and the directories `makechrootpkg` makes. A
symlink planted in the copy by an earlier step, by the base, or by anything
the worker wrote fails the build instead of redirecting a root write.

### The container

One `systemd-nspawn` invocation per container step, from a fixed list:

```
--quiet --directory=<copy> --register=no --keep-unit --as-pid2
--console=pipe --timezone=off --machine=aurcache-<job>
--setenv=PATH=/usr/local/sbin:/usr/local/bin:/usr/bin
--restrict-address-families=          (systemd 261 and later)
--tmpfs=/tmp:<opts>
--bind=<per mount, from BuildSpec.mounts>
```

`--keep-unit` is unconditional: the build's cgroup (`cgroup.rs`) is the
cgroup the runner is started in, so the container stays inside it, and
Stop and `memory.peak` cover it. That retires both the patch and
`docker/nspawn-wrapper.sh`.

### Sources are fetched inside the container

The in-container script (written into the copy as `/aurcache-build`, root
owned, mode 0755) does, as `builduser`:

1. `makepkg --verifysource -o` with `SRCDEST=/srcdest`, `GNUPGHOME` on the
   bound keyring replica, `SSH_AUTH_SOCK` on the bound agent socket. On
   failure, with a non-empty `/srcdest`, clear it and retry once: the rule
   the patch has today.
2. `makepkg` with the job's flags, as `_chrootbuild` does today.

So the PKGBUILD never runs on the host. It runs where `build()` already ran,
under the same user, with the same network. Landlock is no longer needed for
builds, and the two Landlock patches, the `TMPDIR` patch and the `source
PKGBUILD` call go. The server's parse keeps `aurcache-sandbox`; nothing about
it changes.

What the fetch loses: the host's view. A source that only the worker host
can reach (a path, a host-only credential) stops working. The ssh-agent
already reaches the container (`job.rs`, for clones in `prepare()`), and the
keyring replica can be bound read-only, so `git+ssh` and signed sources keep
working. Anything else is to find out in step 2.

### Collecting results

`pkgdest` and `logdest` are opened beneath the copy as above. Each entry
must be a regular file with a name matching what makepkg produces; it is
copied, not moved, into the worker's upload staging, owned by the worker.
Nothing is `chown`ed in place, so no name the build chose reaches a root
`chown`.

### User namespaces

From step 3, `--private-users=pick --private-users-ownership=map` (or
`=auto` where idmapped mounts are unavailable): root in the container is an
unused uid range, so `sudo pacman` in the build, a pacman hook in a crafted
package, or anything else that reaches root in there is not the worker's
root. Binds become idmapped (`--bind=<src>:<dst>:idmap`).

What needs checking, on a pool snapshot:

- idmapped mounts on btrfs subvolumes and snapshots (kernel 5.15 and later
  in principle);
- that the copy is shifted, not chowned: `ownership=map` maps the snapshot's
  root-owned files without writing them, so a snapshot stays cheap;
- the source cache and build tree, owned by `builder` on the host, appearing
  as `builduser` inside;
- the kept-build command (`systemd-nspawn -D kept-<id>
  --bind=...`) needing the same flags to see the same ownership.

### What is the same

`.BUILDINFO` records the build tool. devtools exports
`BUILDTOOL=devtools` and its package version (`lib/common.sh`), and
`makechrootpkg` carries both into the container for makepkg to record. The
runner sets `BUILDTOOL=aurcache` and the worker's version instead, which is
more honest than claiming devtools. Tools that treat `devtools` specially
(rebuilderd, reproducibility checks) would see the difference; for packages
served from an AURCache repository that is the accurate answer.
`SOURCE_DATE_EPOCH` passes through as it does now.

---

## Steps

1. **Parity.** The runner, in `aurcache-chroot`, reproducing today's
   behaviour step for step: downloads still on the host, Landlocked, but
   through the runner instead of the patched script. Selected by a worker
   setting; `makechrootpkg` stays the default. The e2e suite runs both and
   compares packages and `.BUILDINFO` apart from the build tool fields.
2. **Downloads in the container.** Drop the host-side fetch. Watch for
   sources that fail only this way; that is the open question above.
3. **User namespaces**, after a `test-kernel.sh` spike on the points above.
4. **Into the helper.** The runner becomes the root helper's `start-build
   <job-id>`. The sudoers file shrinks to the helper and `(builder) ALL`,
   and the worker's unit can take the hardening its comment rules out today.
5. **Remove** `patch-makechrootpkg.py`, the pacman hook, the patched copy,
   `docker/nspawn-wrapper.sh`, `WORKER_MAKECHROOTPKG` and the parity
   setting.

The container images take the same path at the same time; they call the
same worker code.

---

## Interactions

- `sandbox-attempt-awareness.md` counts the worker's fetch phase as one of
  the sandboxed places. After step 2 there is no host-side fetch for it to
  observe on the worker; the server's parse remains.
- `persistent-build-directory.md` and `btrfs-snapshots.md`: the build tree
  bind and the kept-build layout are unchanged; the kept-build command gains
  the user-namespace flags in step 3.
- `stale-shared-pacman-cache.md`: the shared cache is bound read-only
  explicitly instead of through `CacheDir`; its reconcile is unchanged.
- `unprivileged-workers.md`: option F's helper is this runner plus the pool
  verbs.

## Open questions

- Do any AUR packages rely on fetching from the worker host itself? A
  `file://` source, or credentials only the host has.
- `mkarchroot`: replace it in the helper (`pacman --root` in a private mount
  namespace), or keep it behind a verb that takes no job input?
- Does `namcap` (`-n`, unused today) belong in the runner, as an optional
  report? `package-suspicion-signals.md` might want it.
