# Confining PKGBUILD parsing without Landlock

Status: **Implemented, unreleased** · Last updated: 2026-10-03

## Why

The server sources PKGBUILDs, and used to refuse unless `aurcache-sandbox`
could confine it with Landlock ABI v6 (Linux 6.12). That holds on TrueNAS
25.10 (6.12, `landlock` in the LSM list, checked) but not on Synology, whose
DSM 7.2/7.3 kernels are 4.4 or 5.10 -- no Landlock at all. Upstream users run
the image on both, and every existing install is a Docker one.

Skipping the parse is not an option. `checkout_and_parse`
(`aurcache-utils/src/snapshot.rs`) already prefers a checked-in `.SRCINFO`, so
the server only sources a PKGBUILD when nothing else can answer: a **patched**
package, whose `.SRCINFO` is regenerated from the patched PKGBUILD, and a git
source with no usable `.SRCINFO`.

## Where the complexity lives

A requirement: the rest of AURCache does not care how a parse is confined, or
that it is. Every mechanism lives in `aurcache-sandbox/src/parse.rs`, plus
packaging for the parts that are deployment.

The server states what a parse may do, and nothing about how:

```
aurcache-sandbox parse --pkgbuild FILE [--private PATH]... [--network] -- PROGRAM [ARGS]...
```

`Bridge::command` (`aurcache-utils/src/pkgbuild.rs`) builds exactly that. It
knows which of its paths are private and whether `parse_network` is on; it no
longer clears the environment, picks Landlock flags or reasons about kernels.
The sandbox appends the path the program reads the PKGBUILD from, since that
depends on the mechanism.

The sandbox's old `--no-net` / `--isolate-ipc` flags had no other user and are
gone; the worker's build profile (`--allow-build-env`) is unchanged and stays
Landlock-only, as the chroot worker is new and may require a new kernel.

## The profile, and how

A parse reads its PKGBUILD and nothing under a `--private` path, cannot write
the server's files, cannot signal or inspect other processes, sees only
`PATH`, `HOME` and `LANG`, and cannot reach the Docker socket. Denying TCP
(unless `--network`) is hardening on top, where the kernel can.

Two cases, decided by whether the sandbox runs as root:

**As root -- the container images, so every existing install.** The parse
runs as `aurcache-parse`, and ordinary permissions hold the profile on any
kernel:

- another uid cannot signal the server or read its `/proc/PID/environ`;
- with `setgroups(0)` it is not in the `docker` group, so the socket
  (`root:docker 0660`) is closed to it;
- every `--private` path must be closed to other users and not owned by the
  parse user, checked before every parse; one that is not refuses the parse,
  naming the path and its mode. The owner check is what NFS `root_squash`
  needs (root's files land as `nobody`) -- and why the parse user is a
  dedicated one rather than `nobody`;
- the PKGBUILD is copied, as root, into a memfd owned by the parse user (mode
  0400), and the program reads `/dev/fd/N`: the server's directories stay
  closed to the parse, and nothing is left to clean up. The path must not
  resolve through a symlink, or a checkout could have root copy the database
  in;
- then `setresgid`/`setresuid`, verified, and cwd `/`.

Landlock is layered on top, best effort, as far as the kernel goes: no writes
anywhere (v1), no TCP unless `--network` (v4), no signals or abstract sockets
across the sandbox (v6). Reads are left to the permissions -- a read policy
would have to make an exception for the memfd behind `/dev/fd`.

**Unprivileged -- a native install, whose server runs as `aurcache`.** Landlock
is all there is, so it must hold everything, and is a hard requirement: reads
everywhere but the private paths (plus the PKGBUILD's directory, which may lie
inside one), writes nowhere but `/dev/null`, `truncate` denied (v3: below it a
parse running as the server's own uid could empty the database by path), TCP
denied unless allowed (v4), signals and abstract sockets scoped (v6). So
Linux 6.12, as before -- a new install may have a new requirement. A private
path whose parent does not exist yet (a server that has not created `./data`
for its CA) is covered from its first missing component, which never protects
less than asked.

### Considered: seccomp

A seccomp filter would deny signals and sockets on any kernel, which is what
first made the separate user only a fallback. It was dropped: the pure-Rust
filter compiler (`seccompiler`) lacks armv7, the `libseccomp` crate needs a
per-architecture C library the hybrid image's Arch cross-build does not have,
and the remaining option was hand-assembled BPF. Running every root-launched
parse as the separate user covers signals and the Docker socket without it.
What is given up is TCP denial on kernels older than 6.7 (Synology), which
the docs already called hardening: a parse there can reach the API on
localhost (open to the LAN anyway without OAuth, useless without a token with
it), the worker port (needs a certificate it cannot read) and Postgres (no
credentials in its environment).

## Private paths

The server passes its state directories one by one, not its working directory:
`./db`, the CA dir, the build logs, the source cache (every checkout, private
sources included), `/etc/aurcache`, and `AURCACHE_PROTECTED_DIR`. The
repository is public, and nginx reads it as another user, so `/app` itself
cannot be closed.

Their modes are deployment, not code: `docker/private-state.sh` sets them to
0700 at every start of either image, which also fixes a volume made by an
older image. No Rust outside the sandbox sets a mode. The host requirement is
that secrets it mounts in are not world-readable and nothing the server relies
on is world-writable.

`aurcache-parse` comes from `useradd` in the server image (Debian) and from
`aurcache-server.sysusers` in the hybrid image, which installs the package. A
native server runs unprivileged and never uses it.

## Testing

- Unit: argument parsing, the mode and owner check, missing-ancestor coverage.
- `aurcache-utils`' parse tests run against whichever case the host gets. CI
  runs them unprivileged (Landlock alone), as root (separate user + Landlock),
  and as root with `AURCACHE_SANDBOX_DISABLE=landlock` (separate user alone,
  skipping only the TCP test, which that case does not promise). All three
  passed here, the root ones in a container.
- Probed by hand on every case: no reading a private file, no `kill`, the
  environment scrubbed, an open or parse-owned private dir refused, a
  symlinked PKGBUILD refused; writes and TCP denied wherever Landlock is.

## Open

- Images not yet built and run end to end with this.
- **Synology builds** were the other half: the chroot worker needs 6.7 (btrfs
  simple quotas). Settled separately by making the hybrid image's builder the
  legacy container one in both modes, with Podman inside the container for
  DinD as the old image had -- so the hybrid image keeps the old requirements.
