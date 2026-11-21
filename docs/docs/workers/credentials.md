---
sidebar_position: 5
---

# Build credentials

Some packages fetch their sources from somewhere that requires authentication.
The AUR's `unreal-engine`, for example, clones
`git+ssh://git@github.com/EpicGames/UnrealEngine`, which only works for a GitHub
account that has joined the Epic Games organisation.

A worker holds SSH keys for such builds:

- **its default key**, which every build it runs is offered, unless the
  build's package names another;
- **named keys**, each offered only to the builds whose package names it with
  the **Build SSH key** setting.

Because every build is offered the default key, it should reach nothing
private. Put private access on a named key, set on the packages that need it,
and route those packages to the worker that holds it.

## The default key

### Generated: a key per worker

On first start a worker generates an ed25519 keypair and logs the public half:

```
INFO Build SSH key: /var/lib/aurcache-worker/ssh/id_ed25519
    Add this public key to the account that may fetch restricted sources:
    ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... aurcache-worker
```

This suits GitHub: a public key may be attached to only one account, but an
account can hold many keys. One key per worker is the shape GitHub expects, and
no private key is ever copied between machines — retiring a worker is deleting
one public key, not rotating a shared secret everywhere.

:::warning
`WORKER_DATA_DIR` must be a persisted volume. The generated key lives there, and
if the directory is lost the worker generates a new key that the remote will
reject — with no obvious sign of why.
:::

### Supplying your own

When the credential is issued centrally, or you want deployment to need no
follow-up action, point the worker at a key:

```yaml
services:
  worker:
    environment:
      WORKER_GIT_SSH_KEY: /run/secrets/build_key
    secrets:
      - build_key

secrets:
  build_key:
    file: ./secrets/id_ed25519
```

A plain read-only mount works equally well:

```yaml
    volumes:
      - ./secrets/build-ssh:/etc/aurcache-worker/ssh:ro
    environment:
      WORKER_GIT_SSH_KEY: /etc/aurcache-worker/ssh/id_ed25519
```

When this is set, **no key is generated** — two keys on one machine would make
"which one is in use?" ambiguous exactly when you are debugging an
authentication failure. If the file is missing the worker says so plainly rather
than quietly falling back to a generated key.

Do **not** pass the key through an environment variable. It would be visible in
`docker inspect` and `/proc/<pid>/environ`, and would end up in the compose file
you commit. A mount is both safer and less trouble.

The worker reads the key once, when it starts, into an ssh-agent, and no build
is ever given the file. Replacing the key takes effect when the worker
restarts.

### Keep the file from builds

A key's file must be one no build can read. Builds run PKGBUILD code on the
worker, outside their chroot, as the build user, and a file that user can read
it can copy and keep, while the agent only ever signs. So the file must be
**the worker's own, mode `0600` or `0400`**: a regular file owned by the user
the worker runs as, which is not the build user (`aurcache` in the images:
`docker compose exec worker id -u` prints its uid). A symlink is judged by the
file it leads to. The generated key is already such a file.

Only the owner and mode are checked, so keep a key on a local filesystem: an
NFSv4 ACL (ZFS with `acltype=nfsv4`, for one), a FUSE mount without
`default_permissions` or a CIFS mount with `noperm` can let a build read a
file its mode keeps out, and the worker cannot see that. A POSIX ACL is seen:
its mask shows in the group bits.

The worker checks when it starts. A default key a build could read is warned
of, since every build is offered that key anyway; a named one stops the worker
from starting (see [Named keys](#named-keys)):

```
WARN A build can read the default SSH key's file itself, not only use the key through its agent, and so keep the key: /run/secrets/build_key belongs to uid 1000 and has mode 0644: it is not the worker's own (uid 973). To keep it from builds, make it a regular file of the worker's own, with mode 0600 or 0400
```

A secret mounted root-owned and readable by everyone, as a Kubernetes secret
volume is by default, fails this, and rightly: every build could read it. Nor
can a secret volume be made to pass: its files stay root's whatever the
volume's mode, and `defaultMode: 0400` only makes them root's alone, which is
still not the worker's own. Copy the key into the data volume instead, as a
file of the worker's own (below; an init container can do it), and then do not
mount the secret volume in the worker's container. A compose secret given by
`file:` is a bind mount of that file, with the owner and mode it has on the
host: Docker Compose ignores a secret's `uid`, `gid` and `mode` outside Swarm.
So set them on the host, to the worker's uid in the container:

```sh
sudo chown 973 ./secrets/id_ed25519   # the uid `id -u` printed in the worker
sudo chmod 0600 ./secrets/id_ed25519
```

Under Swarm (`docker stack deploy`), the long syntax sets them instead:

```yaml
    secrets:
      - source: build_key
        uid: "973"
        mode: 0400
```

Or copy the key into the worker's data volume, owned by the worker's uid with
mode `0600`, and point the worker at the copy. Mount the original nowhere in
the worker then: left mounted and readable, it is as readable to builds as
before.

## Named keys

A key that only some packages may use is named on the worker, and the
packages name it on the server.

On the worker, `WORKER_GIT_SSH_KEYS` lists `name:path` pairs, comma separated:

```yaml
services:
  worker:
    environment:
      WORKER_GIT_SSH_KEYS: epic:/run/secrets/epic_key
      WORKER_PACKAGES: unreal-engine
    secrets:
      - epic_key
```

Each file must be kept from builds as the default key's is (see [Keep the
file from builds](#keep-the-file-from-builds)): here, `./secrets/epic_key` on
the host owned by the worker's uid, mode `0600`. Otherwise every build could
read the key, not only those whose package names it, and the worker refuses to
start:

```
Error: refusing to start: a build could read the file of a key WORKER_GIT_SSH_KEYS names, and so keep the key, whichever package it is for. SSH key "epic": /run/secrets/epic_key belongs to uid 1000 and has mode 0644: it is not the worker's own (uid 973). For each, make it a regular file of the worker's own, with mode 0600 or 0400
```

On the server, set **Build SSH key** to `epic` for the packages that may use
it: on each package's page under **Build config**. A global value, on the
**Settings** page or with `BUILD_SSH_KEY` on the server, applies to every
package without one of its own. Empty, at either level, means "what applies
anyway": the global value for a package, `default` for the server.

Onboarding a named key is then:

1. Add `name:path` to `WORKER_GIT_SSH_KEYS` on the worker, with the file kept
   from builds, and restart it.
2. Add the key's public half to the account that has access to the sources.
3. Set **Build SSH key** to that name on the packages that need it.
4. Set `WORKER_PACKAGES` on that worker to the same packages, so their builds
   are routed to it. See [Routing](./routing.md).

`WORKER_PACKAGES` matters: a package that names a key builds on whichever
worker claims it, and a worker that does not hold the key builds it without
one. Its log says so before anything else:

```
[worker] this worker holds no SSH key named "epic", which the build_ssh_key setting names for unreal-engine; building without one
```

A package that fetches nothing over SSH builds as usual. One that does fails at
that fetch, with this line above the failure.

A name is a label, never a path: a lowercase letter or digit, then up to 63
more of those, `_` or `-`. `default` is the default key's, and cannot be
named in `WORKER_GIT_SSH_KEYS`. An entry the worker cannot use — no such name,
`default`, a name given twice, or no path — is dropped with a warning when it
starts. Only the name reaches the worker; the key itself never leaves it. The
server refuses to store anything that is no name, and a `BUILD_SSH_KEY` in its
environment that is no name reads as `default`, with a warning in its log.

`WORKER_GIT_SSH_KEYS` is read from the worker's environment, and is not one of
the settings the server can change for a worker. Each key is read once, when
the worker starts: replacing one takes effect at the next restart.

:::warning
Named keys take a kernel whose Landlock can restrict UNIX sockets: Linux 7.1 or
later (Landlock ABI 9). On an older kernel nothing keeps a build from another
key's agent while its PKGBUILD code runs outside the chroot (see [What a build
can do with a key](#what-a-build-can-do-with-a-key)), so the worker holds no
named key there at all, only its default one, and says why when it starts:

```
ERROR WORKER_GIT_SSH_KEYS names SSH keys, but builds could not be kept from their agents outside their chroot, because aurcache-sandbox, asked as the build user, says it would not: kernel (this kernel's Landlock cannot restrict UNIX sockets: that takes Landlock ABI 9, Linux 7.1 or later): this worker holds none of them, only its default key, and a build whose package names one goes ahead without it
```

The worker does not judge this itself. It asks the sandbox builds run under,
as the build user:

```sh
aurcache-sandbox --capabilities --agent-dir <WORKER_DATA_DIR>/agent
```

It holds named keys only when the answer is `resolve-unix`. The sandbox
answers `no-resolve-unix:` and its reason instead on an older kernel, and
also when:

- `/etc/aurcache/sandbox-protected` exists and:
  - the build user cannot read it, as at `0600 root`;
  - it is anything but a regular file;
  - it is, or is reached through, a symlink to nothing;
  - it is larger than 1 MiB;
  - or it names a path under a directory that does not exist.

  No build starts at all then, not only those naming a key. Keep the file
  readable by everyone: it lists paths, not secrets.
- that file is missing or lists nothing, so builds are kept from no path, or
  it does not protect the agents' directory, `<WORKER_DATA_DIR>/agent`, where
  it really is: listing it, or a directory above where it really is, does.

The log says which. When the sandbox could not be asked, it gives how the
question failed and what was printed: an `aurcache-sandbox` too old to know
the question (update the package) refuses it as a usage error, exit status 2.
A sandbox that does not answer within 5 seconds counts as one that could not
be asked.

Where the worker holds named keys, it says:

```
INFO Builds are refused the agents of the SSH keys they are not offered outside their chroot as well (Landlock)
```

It asks again as each build starts. Should the answer change, as with the
policy file edited or the sandbox package replaced under a running worker, or
should the question fail, the worker stops the named keys' agents, removing
their sockets, and says why in its log, once. From then on it holds only its
default key until it restarts: a build whose package names a key goes ahead
without it, with a line in its log.

The check is made when a build starts, not while builds run: a change takes
effect for the builds that start after it. A build already under way is not
checked again, and its PKGBUILD code outside the chroot runs under the policy
as it reads by then, which may leave the agents within its reach until the
next build start stops them. A fetch that needs the key after its agent is
stopped fails, and the worker's log says why the agents were stopped. Change
the policy or the sandbox with no builds running.
:::

## Host keys

With no `known_hosts` configured, the worker accepts a host key the first time
it sees one and rejects it if it later changes. To pin known hosts instead:

```sh
ssh-keyscan github.com > known_hosts
```

```yaml
    environment:
      WORKER_SSH_KNOWN_HOSTS: /etc/aurcache-worker/ssh/known_hosts
```

A host that file does not name is then refused: the fetch fails with `Host key
verification failed`. Nothing else can admit one. ssh is told to ignore the
system's `/etc/ssh/ssh_known_hosts`, any `KnownHostsCommand` and host keys
published in DNS, and never to accept a host on first use, in options that win
over any `ssh_config`.

Before each build offered a key the worker copies the file to
`<WORKER_DATA_DIR>/ssh-known-hosts/known_hosts`, so the original only has to be
readable by the worker; a `0600` secret is fine. A replaced file takes effect at
the next build. The copy is the worker's own file, which builds can read and the
build user cannot change. It is bound read-only into the build's chroot, at the
same path, for a PKGBUILD that clones in `prepare()`. Each build gets a fresh
copy renamed over the last, so nothing a build leaves at that path reaches a
later build.

If the file cannot be read, every build offered a key fails until it can,
with an error naming `WORKER_SSH_KNOWN_HOSTS`: the worker never falls back to
accepting hosts on first use.

## What a build can do with a key

Builds never hold a key. The worker loads each key into an ssh-agent of its
own, which runs as the build user, and what a build can reach is the one agent
it is offered, never a key:

- **No key file ever enters the chroot**, for any build. Each stays on the
  worker, which reads it once into its agent; a generated key stays in the
  worker's data directory, which the build user cannot read, and a supplied
  one must be kept from that user too ([Keep the file from
  builds](#keep-the-file-from-builds)).
- **A build reaches the agent it is offered inside its chroot**, as well as
  for the source download outside it. That agent's directory is bound
  read-only into the chroot at the same path, and `SSH_AUTH_SOCK` in the
  build's `makepkg.conf` names its socket, so `prepare()`, `build()` and
  `package()` can have the agent sign SSH logins: a PKGBUILD that clones from
  an authenticated remote in `prepare()`, rather than declaring it in
  `source=()`, works. Declared sources are still the better path — they are
  what gets checksummed, and what gets fetched before any package code runs.
  The agent signs; it never hands the key back.
- **The bind is read-only**, so a build can connect to the socket but cannot
  remove or replace it for the next build: root in the build's container runs
  without `CAP_SYS_ADMIN` and cannot remount it writable, and a user namespace
  a build makes for itself cannot either.
- **Every other agent stays out of the chroot.** Each has a directory of its
  own, and only the offered one is bound. A build offered no key, as one whose
  package names a key the worker does not hold, gets no bind and no
  `SSH_AUTH_SOCK`, in its `makepkg.conf` or its environment.

What this guarantees:

- A private key never leaves the worker. It is not sent to the server, not
  stored there, and not in any job; only the key's name travels.
- Builds get only an agent, which signs SSH logins and nothing else. Nothing a
  build captures outlives it.
- A build cannot reach another key's agent by its path either. Every build
  runs as the same build user, which the agents belong to, so file permissions
  cannot keep one build from them; the sandbox around the PKGBUILD code that
  runs outside the chroot does. It refuses a connection to any UNIX socket
  beneath the paths the worker keeps private, the agents' directory among
  them, and lets a build connect to the socket of the agent it is offered
  alone. This holds while other builds are using the agents.

The last point takes Landlock's `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` (Landlock
ABI 9, Linux 7.1 and later), an `aurcache-sandbox` that has the rule, and a
sandbox policy that lists the agents' directory. On an older kernel, with an
older sandbox, or without that entry, the agents' sockets are not kept from
builds, and the worker holds no named key there; under a policy the build
user cannot read, no build runs at all. Should that change under a running
worker, it stops the agents of the named keys it holds at the next build
start.

What it does not:

- While a build runs, its code can log in with the key it is offered to any
  host that key can reach. With the default key that is every build, so give
  it access to nothing you would not trust every package with. Name a key
  only on packages whose PKGBUILDs you trust with its access, and whoever can
  edit them on the server: changing such a package's source, or a patch to
  its PKGBUILD, changes what runs with the key.
- Trusting a package with a key also means trusting the dependencies of it
  that AURCache builds. Their binaries, and whatever their install
  scriptlets leave in its chroot, such as an `/etc/profile.d` script, run
  there while it builds, with the agent bound. So they can use the key as
  well as its own PKGBUILD can.
- What a key fetches is cached for the next build, in the worker's source
  cache (`srcdest/<pkgbase>` in the `cache` subvolume of its storage pool).
  The PKGBUILD code of every other build on that worker can read it, while
  it runs outside its chroot. Give packages that name a key a worker of their
  own, where nothing you would not show those sources builds:
  [`WORKER_PACKAGES`](./routing.md) sends such a package to that worker, but
  does not keep other packages off it.
- A worker that does not read the key name a job carries offers its default
  key to every build it runs, whatever the server's setting says.

## On the Docker worker

The Docker worker takes `WORKER_GIT_SSH_KEY` and `WORKER_SSH_KNOWN_HOSTS`
too, and keeps the key out of the build the same way: it stays in an
`ssh-agent` of the worker's own, which a build reaches over a link while
makepkg runs. A build can have the key sign while it runs, a login or anything
else, and so can whatever a dependency's install script (which runs as root,
before the build) left there for it to run; neither can copy the key. So the
key should be one made for builds, and not registered as a signing key
anywhere. The differences:

- nothing is generated: a key must be given;
- it holds that one key, as its default, and no [named keys](#named-keys)
  (`WORKER_GIT_SSH_KEYS` is not read): a build whose package's **Build SSH
  key** names another is offered no key, and its log says so;
- `known_hosts` is required, since a container is new for every build and an
  ssh that trusts the first key it is shown would trust any host.

The ssh git runs in the build (`GIT_SSH_COMMAND`) is given the same options as
on the chroot worker, so that file alone says which hosts git trusts, whatever
the builder image's `ssh_config` and `/etc/ssh/ssh_known_hosts` say. That pins
git's ssh against the image's configuration, and nothing more: a PKGBUILD can
set `GIT_SSH_COMMAND` itself or run `ssh` directly without these options, and
a dependency's install script runs as root there and could replace `ssh`
itself for the rest of the build.

See [Docker worker](./docker.md#build-credentials).

## Other credentials

For credentials that are not SSH — a `.netrc`, an API token, a licence file —
`WORKER_BIND_MOUNTS` exposes extra paths to the build:

```sh
WORKER_BIND_MOUNTS=/etc/aurcache-worker/licence:/opt/licence
```

Unlike the SSH keys these **are** readable by package code, and by every build on
the worker. Use them only for credentials you are willing to expose to all the
packages that worker builds.
