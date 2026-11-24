---
sidebar_position: 6
---

# SSH package sources

A package can be added from a git repository as well as from the AUR, and a
private repository is usually reached over SSH:

```bash
aurcli pkg add git@github.com:you/private-pkg.git
aurcli pkg add ssh://git@git.example.com:2222/srv/pkg.git --ref main
```

The add dialog takes the same URLs. The server reads these forms of git URL,
and refuses any other before connecting:

- `ssh://[user@]host[:port]/path`; with no account it logs in as `git`, which
  is what every forge expects;
- the scp-like `user@host:path`, which has no port (write `ssh://` for one);
- `https://`, `http://`, `git://` and `file://` URLs, and a local path with no
  colon in it.

The host is a name or an IPv4 address: an IPv6 literal, git's bracketed
spellings, `git+ssh://` and a password in the URL are refused. Host aliases
from an `ssh_config` are not read: write the real host and port in the URL.

## The server's key

The server clones the repository itself, to read its PKGBUILD before anything
is built, so it has an SSH key of its own. On first start it generates an
ed25519 keypair and logs the public half:

```
INFO SSH key for package sources: ./data/ssh/id_ed25519
    Add this public key to the git remotes that AURCache clones over SSH:
    ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... aurcache-server
```

The same key is on the **Settings** page, and `aurcli ssh key` prints it, the
key alone on the first line. Add it to the repository as a **deploy key**
(read-only access is all it needs), or to an account that can read the
repositories you build from.

Anyone who can use the API can add a package from any URL, and so make the
server clone any repository that this key can read, and build what is in it.
Give the key only the repositories that you mean AURCache to build: added to
several repositories, or to an account that reads many, it gives every API user
read access to all of them through the server. Likewise any API user can have the
server connect over SSH to any `host:port` it can reach, and with the default
host-key policy (below) each new host is recorded for good; the activity log
is where to look for hosts that nobody meant to add.

The key lives in `AURCACHE_SSH_DIR` (`./data/ssh`, so `/app/data/ssh` in the
images), which must be persisted: a server that loses it generates a new one,
and every remote you added the old one to then refuses it. A dump taken with
secrets carries the key and `known_hosts`, and restoring it puts them back:
see [Backup and restore](../cli/maintenance.md).

This key is the server's, for a package's own repository. The sources a
PKGBUILD downloads while building (its `source=()` array, or a clone in
`prepare()`) are fetched by the workers, with keys of their own; see [Build
credentials](../workers/credentials.md). The server never offers its key to
those sources, not even to check them for new commits.

### Supplying your own key

```yaml
services:
  aurcache:
    environment:
      AURCACHE_GIT_SSH_KEY: /run/secrets/aurcache_ssh_key
    secrets:
      - aurcache_ssh_key
```

When `AURCACHE_GIT_SSH_KEY` is set, no key is generated, and a missing file is
reported as such at start rather than replaced. The key must be in OpenSSH's
format (what `ssh-keygen` writes) and have no passphrase: a deploy key kept
`0600` is what this is for.

### Keeping the key from PKGBUILDs

PKGBUILDs are parsed in a sandbox that cannot read `AURCACHE_SSH_DIR` or the
key that `AURCACHE_GIT_SSH_KEY` names. The images run the server as root, so
the sandbox parses as a user of its own and relies on file permissions, on any
kernel: before every parse it checks that both are closed to other users, and
refuses to parse while either is open. Keep the directory `0700`, and the key
`0600` or `0400`.

## Host keys

The server never trusts an SSH host without saying so. (The worker checks the
hosts that a build fetches from, against its own `known_hosts`: see
[Build credentials](../workers/credentials.md#host-keys).)

**By default**, the first time it connects to a host it records the host's key
in `AURCACHE_SSH_DIR/known_hosts`, and says so in the activity log (the **Logs**
page, *SSH host trusted on first use*) and the server log:

```
WARN trusted the SSH host key of github.com on first contact, with nothing to
    check it against: ecdsa-sha2-nistp256 SHA256:p2QAMXNIC1TJYWeIOttrVc98/R1BUFWu3/LiyKgUfQM.
    Compare it with the fingerprint that the host publishes; any other key from
    github.com is refused from now on
```

Nothing vouched for that key, so check it against the fingerprint that its
host publishes: GitHub's are under *GitHub's SSH key fingerprints* in its
documentation, and GitLab.com's under *SSH host keys fingerprints* in GitLab's.
On a server of your own, `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`
prints it. From then on, a host that presents a different key is refused, with
a message naming the key that it presented and the ones recorded.

**To trust only hosts you list**, give the server a `known_hosts` file:

```sh
ssh-keyscan github.com gitlab.example.com > known_hosts
ssh-keyscan -p 2222 git.example.com >> known_hosts
```

```yaml
    environment:
      AURCACHE_SSH_KNOWN_HOSTS: /etc/aurcache/known_hosts
```

The server then never adds to that file, and refuses any host not in it. Plain
and hashed host names (`ssh-keyscan -H`) and `@revoked` lines are understood;
a wildcard or a negated pattern matches no host, and `@cert-authority` lines
are ignored. Record every key type a host has, which is what `ssh-keyscan`
does without `-t`: the server's SSH library picks the type, and a host known
only by another one is refused.

### When a host's key changes

The refusal names the file. With the server's own file, remove the host's line
and the next attempt records the new key. The file is `known_hosts` in
`AURCACHE_SSH_DIR`:

| Install | File |
|---|---|
| Server image | `/app/data/ssh/known_hosts` |
| Hybrid image | `/app/repo/.ssh/known_hosts` while only the repository is on a volume; `/app/data/ssh/known_hosts` once `/app` is mounted |
| Native package | `/var/lib/aurcache/data/ssh/known_hosts` |

The images have no `ssh-keygen`, so edit the volume's copy of the file from the
host, by hand or with `ssh-keygen -R <host> -f <file>`; `ssh-keygen -R`
rewrites the file owned by whoever runs it, mode `0600`, so give it back to the
server's user afterwards. With your own file, replace the entry with the
host's new key.

## Troubleshooting

- **"refused the server's SSH key"**: the remote does not know the key. Add the
  public key from `aurcli ssh key`, and check that the fingerprint in the
  message is the one you added.
- **"needs the server's SSH key"**: the key could not be loaded at start; the
  rest of the message, and the server log, say why.
- **"is not in AURCACHE_SSH_KNOWN_HOSTS"**: add the host to your file with
  `ssh-keyscan`, run on the host (the images have none).
- **"Someone may be intercepting the connection"**: the host presented a key
  other than the ones recorded. Check with the host's owner before replacing
  the entry.
- **"SSH sources need one built with libssh2"**: a native install links the
  system's libgit2, and this one was built without SSH, or with its `exec`
  backend, which runs `ssh` and would bypass the server's key and its host-key
  checks. Arch's `libgit2` package is built with libssh2, and the container
  images carry their own.
- **"unsupported git URL"**: write the URL in one of the forms above.
- **A rewrite in the server's git configuration** (`url.<base>.insteadOf`) is
  applied after these checks: don't configure one for the remotes that the
  server clones.
