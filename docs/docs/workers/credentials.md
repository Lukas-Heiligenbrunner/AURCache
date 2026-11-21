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
is ever given the file. The file only has to be readable by the worker's user,
and ssh's own checks on a key file's mode never see it. Replacing the key takes
effect when the worker restarts.

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

On the server, set **Build SSH key** to `epic` for the packages that may use
it: on each package's page under **Build config**. A global value, on the
**Settings** page or with `BUILD_SSH_KEY` on the server, applies to every
package without one of its own. Empty, at either level, means "what applies
anyway": the global value for a package, `default` for the server.

Onboarding a named key is then:

1. Add `name:path` to `WORKER_GIT_SSH_KEYS` on the worker, and restart it.
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

- the build user cannot read `/etc/aurcache/sandbox-protected` (a symlink
  to nothing, in its place or on the way to it, counts as unreadable rather
  than missing), or the file names a path under a directory that does not
  exist. Either way no build starts at all, not only those naming a key.
  Keep the file readable by everyone: it lists paths, not secrets.
- that file is missing, lists nothing, or does not protect the agents'
  directory, `<WORKER_DATA_DIR>/agent`, where it really is: listing it, or a
  directory above where it really is, does.

The log says which. When the sandbox could not be asked, it gives how the
question failed and what was printed: an `aurcache-sandbox` too old to know
the question (update the package) refuses it as a usage error, exit status 2.
Where the worker holds named keys, it says:

```
INFO Builds are refused the agents of the SSH keys they are not offered outside their chroot as well (Landlock)
```

It asks again before each build. Should the answer change, as with the
policy file edited or the sandbox package replaced under a running worker, a
build whose package names a key goes ahead without it, with a line in its
log, and the worker's log says why once.
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
  worker's data directory, which the build user cannot read.
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
sandbox policy the build user can read that lists the agents' directory. On
an older kernel, with an older sandbox, or without that entry, sockets are not
restricted. With a policy the build user cannot read, every build is refused.
Either way the worker holds no named key there.

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

## Other credentials

For credentials that are not SSH — a `.netrc`, an API token, a licence file —
`WORKER_BIND_MOUNTS` exposes extra paths to the build:

```sh
WORKER_BIND_MOUNTS=/etc/aurcache-worker/licence:/opt/licence
```

Unlike the SSH keys these **are** readable by package code, and by every build on
the worker. Use them only for credentials you are willing to expose to all the
packages that worker builds.
