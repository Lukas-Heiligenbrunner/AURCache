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

The add dialog takes the same URLs. Both spellings git accepts work: `ssh://`
URLs (also `git+ssh://`), and the scp-like `user@host:path`. A URL that names
no account logs in as `git`, which is what every forge expects. Host aliases
from an `ssh_config` are not read: write the real host and port in the URL.

## The server's key

The server clones the repository itself, to read its PKGBUILD before anything
is built, so it has an SSH key of its own. On first start it generates an
ed25519 keypair and logs the public half:

```
INFO SSH key for package sources: ./data/ssh/id_ed25519
    Add this public key to the git remotes AURCache clones over SSH:
    ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... aurcache-server
```

The same key is on the **Settings** page, and `aurcli ssh-key` prints it, the
key alone on the first line. Add it to the repository as a **deploy key**
(read-only access is all it needs), or to an account that can read the
repositories you build from.

The key lives in `AURCACHE_SSH_DIR` (`./data/ssh`, so `/app/data/ssh` in the
images), which must be persisted: a server that loses it generates a new one,
and every remote you added the old one to then refuses it.

This key is the server's, for a package's own repository. The sources a
PKGBUILD downloads while building (its `source=()` array) are fetched by the
workers, with keys of their own: see [Build credentials](../workers/credentials.md).
The server never offers its key to those, not even to check them for new
commits.

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
reported as such at start rather than replaced. The key must have no
passphrase: the server has nobody to ask for one. It is best in OpenSSH's own
format (what `ssh-keygen` writes); a key in an older format needs its `.pub`
beside it.

PKGBUILDs are parsed in a sandbox that cannot read `AURCACHE_SSH_DIR` or the key
`AURCACHE_GIT_SSH_KEY` names. On a kernel without Landlock the sandbox relies on
file permissions instead, and refuses to parse anything while the key is
readable by other users: keep it `0600`, or `0400`.

## Host keys

The server never trusts an SSH host without saying so.

**By default**, the first time it connects to a host it records the host's key
in `AURCACHE_SSH_DIR/known_hosts` and logs it:

```
WARN Trusting the SSH host key of github.com on first contact:
    ecdsa-sha2-nistp256 SHA256:p2QAMXNIC1TJYWeIOttrVc98/R1BUFWu3/LiyKgUfQM, recorded in ./data/ssh/known_hosts.
    Any other key from github.com will be refused.
```

Compare the fingerprint with the one the host publishes (GitHub's are under
*GitHub's SSH key fingerprints* in its documentation). From then on, a host
that presents a different key is refused, with a message naming both keys.

**To trust only hosts you list**, give the server a `known_hosts` file:

```sh
ssh-keyscan github.com gitlab.example.com > known_hosts
ssh-keyscan -p 2222 git.example.com >> known_hosts
```

```yaml
    environment:
      AURCACHE_SSH_KNOWN_HOSTS: /etc/aurcache/known_hosts
```

The server then never adds to that file, and refuses any host not in it. Hashed
entries (`ssh-keyscan -H`), wildcards and `@revoked` lines are understood;
`@cert-authority` lines are ignored.

Record every key type a host has, which is what `ssh-keyscan` does without
`-t`. The server negotiates whichever of the host's key types its SSH library
prefers, and a host known only by another type is refused, as a changed key
would be.

### When a host's key changes

The refusal says what to run. With the server's own file, remove the old entry
and the next attempt records the new key:

```sh
ssh-keygen -R github.com -f /app/data/ssh/known_hosts
```

With your own file, replace the entry with the host's new key.

## Troubleshooting

- **"refused the server's SSH key"**: the remote does not know the key. Add the
  public key from `aurcli ssh-key`, and check the fingerprint the message gives
  is the one you added.
- **"needs the server's SSH key, which it has none of"**: the key could not be
  loaded at start; the rest of the message, and the server log, say why.
- **"is not in AURCACHE_SSH_KNOWN_HOSTS"**: add the host to your file, with the
  `ssh-keyscan` command the message gives.
- **"records another for it"**: the host presented a key other than the one
  recorded. This is what someone intercepting the connection looks like, so
  check with the host's owner before replacing the entry.
