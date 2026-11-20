---
sidebar_position: 5
---

# Build credentials

Some packages fetch their sources from somewhere that requires authentication.
The AUR's `unreal-engine`, for example, clones
`git+ssh://git@github.com/EpicGames/UnrealEngine`, which only works for a GitHub
account that has joined the Epic Games organisation.

Every worker therefore has an SSH key available to `makepkg` when it downloads
sources.

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
4. Set `WORKER_PACKAGES` for the packages that need it, so those builds are
   routed to this machine. See [Routing](./routing.md).

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

The file only has to be readable by the worker: the worker reads the key and
loads it into an ssh-agent, and no build is ever given the file.

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

Before each build the worker copies the file to
`<WORKER_DATA_DIR>/ssh-known-hosts/known_hosts`, so the original only has to be
readable by the worker; a `0600` secret is fine. A replaced file takes effect at
the next build. The copy is the worker's own file, which builds can read and the
build user cannot change. It is bound read-only into the build's chroot, at the
same path, for a PKGBUILD that clones in `prepare()`. Each build gets a fresh
copy renamed over the last, so nothing a build leaves at that path reaches a
later build.

If the file cannot be read, every build fails until it can, with an error naming
`WORKER_SSH_KNOWN_HOSTS`: the worker never falls back to accepting hosts on
first use.

## The key is not visible to the package

The key file is never mounted into the chroot, where `prepare()`, `build()`
and `package()` run — those are arbitrary scripts from a PKGBUILD, and anything
they can reach they can send somewhere else. What the build gets is the
agent's socket, so package code can have the agent sign with the key while the
build runs, but can never read the key itself.

Two consequences:

- A PKGBUILD that clones from an authenticated remote in `prepare()`, rather
  than declaring it in `source=()`, works: ssh authenticates through the agent.
  Declared sources are still the better path — they are what gets checksummed,
  and what gets fetched before any package code runs.
- Package code can authenticate with the key, to any host, for as long as its
  build runs. Nothing it captures outlives the build.

## Other credentials

For credentials that are not SSH — a `.netrc`, an API token, a licence file —
`WORKER_BIND_MOUNTS` exposes extra paths to the build:

```sh
WORKER_BIND_MOUNTS=/etc/aurcache-worker/licence:/opt/licence
```

Unlike the SSH key these **are** mounted into the chroot and are therefore
readable by package code. Use them only for credentials you are willing to
expose to the packages you build.

They are mounted into the build's container only, not into the ones that
sync the chroot's package databases, upgrade it and install the build's
dependencies before it. So a mount here cannot serve those steps: a local
`file://` repository for the chroot's upgrade cannot be provided through it.
