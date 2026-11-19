---
sidebar_position: 5
---

# Build credentials

Some packages fetch their sources from somewhere that requires authentication.
The AUR's `unreal-engine`, for example, clones
`git+ssh://git@github.com/EpicGames/UnrealEngine`, which only works for a GitHub
account that has joined the Epic Games organisation.

A worker can hold an SSH key for such builds. Two settings decide which builds
may use it:

- on the server, **Build SSH key** (`build_ssh_key`) names the key a package's
  builds may use. It is off by default: no build is offered a key.
- on the worker, `WORKER_GIT_SSH_PACKAGES` optionally lists the only packages
  that worker will offer its key to, whatever the server asks.

A build gets the key only when both allow it.

## Naming the key a package may use

Each worker holds one key, named `default`: the key `WORKER_GIT_SSH_KEY` points
at, or the one the worker generates (see below). Set **Build SSH key** to
`default` for the packages that need it:

- for one package, on its page under **Build config**;
- for every package, on the **Settings** page, or with `BUILD_SSH_KEY=default`
  on the server. A package's own value overrides it, and an empty one gives that
  package no key.

Until the setting names a key, no build is offered one, including on a worker
configured with `WORKER_GIT_SSH_KEY`. A deployment whose packages fetch over
SSH must set it after upgrading the server, globally or per package, or those
builds fail at the source download.

A name is a label, never a path: a lowercase letter or digit, then up to 63
more of those, `_` or `-`. Only the name reaches the worker; the key itself
never leaves it.

A build whose package names a key its worker does not hold goes ahead without
one, and its log says so before anything else:

```
[worker] this worker holds no SSH key named "gitlab", which the build_ssh_key setting names for hello; building without one
```

A package that fetches nothing over SSH builds as usual. One that does fails at
that fetch, with this line above the failure.

## Limiting a worker's key to some packages

`WORKER_GIT_SSH_PACKAGES` is a ceiling set on the worker itself: exact pkgbase
names, comma or space separated, matched as `WORKER_PACKAGES` matches them —
whole names, case-sensitive, no wildcards.

| `WORKER_GIT_SSH_PACKAGES` | Builds offered the key |
|---|---|
| unset | every build the server's setting allows |
| `unreal-engine,epic-tools` | only builds of those packages, when the setting allows them |
| set but empty | none |

Set but empty lists no package, so an empty value never widens who gets the
key. To leave it unset, leave the variable out: in a compose file, a line such
as `WORKER_GIT_SSH_PACKAGES=${SSH_PACKAGES:-}` sets it empty when
`SSH_PACKAGES` is unset, and the worker then offers its key to no build.

It is read from the worker's environment, and is not one of the settings the
server can change for a worker: the server can narrow who gets the key, by not
naming it for a package, but never widen it past this list.

A build that is not offered the key runs without it, and its log says why:

```
[worker] no SSH key offered: hello is not in this worker's WORKER_GIT_SSH_PACKAGES
[worker] no SSH key offered: the build_ssh_key setting names none for hello
```

Its sources are still fetched, without SSH; a source that needs the key then
fails there, after that line.

### A key for a few packages only

For a key meant for specific packages, give the worker that holds it the same
list in both variables:

```yaml
services:
  worker:
    environment:
      WORKER_PACKAGES: unreal-engine
      WORKER_GIT_SSH_PACKAGES: unreal-engine
```

`WORKER_PACKAGES` makes those packages build only on this worker (see
[Routing](./routing.md)); `WORKER_GIT_SSH_PACKAGES` makes them the only builds
here that are offered the key. Set **Build SSH key** to `default` on those
packages too.

## The default: a key per worker

On first start a worker generates an ed25519 keypair and logs the public half:

```
INFO Build SSH key: /var/lib/aurcache-worker/ssh/id_ed25519
    Add this public key to the account that may fetch restricted sources:
    ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... aurcache-worker
```

Onboarding is then:

1. Start the worker and approve it on the Workers page.
2. Copy the public key from its log.
3. Add it to the GitHub account (**Settings → SSH and GPG keys**) that has
   access to the sources.
4. Set `WORKER_PACKAGES` and `WORKER_GIT_SSH_PACKAGES` for the packages that
   need it, and **Build SSH key** to `default` on them.

This suits GitHub: a public key may be attached to only one account, but an
account can hold many keys. One key per worker is the shape GitHub expects, and
no private key is ever copied between machines — retiring a worker is deleting
one public key, not rotating a shared secret everywhere.

:::warning
`WORKER_DATA_DIR` must be a persisted volume. The generated key lives there, and
if the directory is lost the worker generates a new key that the remote will
reject — with no obvious sign of why.
:::

## Supplying your own key

When the credential is issued centrally, or you want deployment to need no
follow-up action, point the worker at a key:

```yaml
services:
  worker:
    environment:
      WORKER_GIT_SSH_KEY: /run/secrets/build_key
      WORKER_PACKAGES: unreal-engine
      WORKER_GIT_SSH_PACKAGES: unreal-engine
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

Before each build offered the key the worker copies the file to
`<WORKER_DATA_DIR>/ssh-known-hosts/known_hosts`, so the original only has to be
readable by the worker; a `0600` secret is fine. A replaced file takes effect at
the next build. The copy is the worker's own file, which builds can read and the
build user cannot change. It is bound read-only into the build's chroot, at the
same path, for a PKGBUILD that clones in `prepare()`. Each build gets a fresh
copy renamed over the last, so nothing a build leaves at that path reaches a
later build.

If the file cannot be read, every build offered the key fails until it can,
with an error naming `WORKER_SSH_KNOWN_HOSTS`: the worker never falls back to
accepting hosts on first use.

## What a build can do with the key

Builds never hold the key. The worker loads it into an ssh-agent that runs as
the build user, and a build that is offered the key is given the agent's socket
— in its `makepkg.conf`, for the source download, and with the agent's
directory bound read-only into its chroot, for a PKGBUILD that clones in
`prepare()`. The agent signs SSH logins; it never hands the key back.

What this guarantees:

- The private key never leaves the worker. It is not sent to the server, not
  stored there, and not in any job; only the key's name travels.
- The build user cannot read the key file, and builds get only the agent, which
  signs SSH logins and nothing else. Nothing a build captures outlives it.
- A build that is not offered the key gets none of it: no socket in its
  `makepkg.conf` or its environment, and no agent mounted into its chroot.
- Nor can such a build reach the agent by its path. Every build runs as the
  same build user, which the agent belongs to, so file permissions cannot keep
  one build from it; the sandbox around the PKGBUILD code that runs outside
  the chroot does. It refuses a connection to any UNIX socket beneath the
  paths the worker keeps private, the agent's among them, and lets a build
  offered the key connect to the agent's socket alone. This holds while other
  builds are using the agent.

The last point takes Landlock's `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` (Landlock
ABI 9, Linux 7.1 and later). On an older kernel the sandbox cannot restrict
sockets: the key still stays out of every chroot that is not offered it, but
PKGBUILD code running outside the chroot can reach the agent by its path. The
worker says which applies when it starts:

```
INFO Builds not offered the SSH key are refused its agent outside their chroot as well (Landlock)
```

The sandbox reads the paths it keeps private from
`/etc/aurcache/sandbox-protected`, which names the agent's directory under the
default `WORKER_DATA_DIR`. A worker whose data directory is elsewhere warns at
startup until its `agent` directory is added to that file.

What it does not:

- While a package that is offered the key builds, its code can log in with the
  key to any host the key can reach. Offer it only to packages whose PKGBUILDs
  you trust with that access.
- A worker that does not read the key name a job carries offers its key to
  every build it runs, whatever the server's setting says.

## Other credentials

For credentials that are not SSH — a `.netrc`, an API token, a licence file —
`WORKER_BIND_MOUNTS` exposes extra paths to the build:

```sh
WORKER_BIND_MOUNTS=/etc/aurcache-worker/licence:/opt/licence
```

Unlike the SSH key these **are** readable by package code, and by every build on
the worker. Use them only for credentials you are willing to expose to all the
packages that worker builds.
