---
sidebar_position: 4
---

# Pacman Repository

Add the following to your `/etc/pacman.conf` on your target machine to use the repo:

```bash
# nano /etc/pacman.conf
[repo]
SigLevel = Optional TrustAll
Server = http://<server_ip>:8081/$arch
```

8081 is the default; a server that sets `AURCACHE_REPO_PORT` serves the
repository on that port instead.

That stanza is for an instance that does not sign its repository, the
default. One that does is used with `SigLevel = Required`, once its key is
trusted; see [Signing the repository](#signing-the-repository).

## Browse the repository

Open `http://<server_ip>:8081/` in a browser to list architecture directories,
then open one to browse and download its packages and repository databases.
The built-in server hides dot-prefixed files and directories, including staged
uploads and temporary databases, and refuses direct requests for those paths.

## Let the CLI fill it in

`aurcli` is already configured to talk to the instance, and the repository
is the same host on the repository port — so it can print the stanza with
nothing left to substitute:

```bash
$ aurcli repo config
[repo]
SigLevel = Optional TrustAll
Server = http://192.168.1.10:8081/$arch
```

Or have it make the change:

```bash
aurcli repo config --install
sudo pacman -Sy
```

`--install` appends the stanza to `/etc/pacman.conf` and is safe to run twice —
if the repository is already configured it says so and changes nothing. It tries
the write unprivileged first, so running as root, or against a file you own, never
prompts; only a permission error falls back to `sudo`, which asks for your
password on the terminal. `--pacman-conf` points it at a different file, for a
container or a chroot.

Without `--install` the stanza goes to stdout and the instructions to stderr, so
it also appends cleanly by hand:

```bash
aurcli repo config | sudo tee -a /etc/pacman.conf
```

With a token configured, it asks the server how it actually publishes the
repository rather than assuming the default port — a deployment behind a reverse
proxy, on another port (`AURCACHE_REPO_PORT` or `AURCACHE_PUBLIC_URL`), or under
a path is described correctly. The host is still
decided locally, because the server only sees how *it* was reached: a published
address that names a real host is kept, and the `localhost` default is replaced
with the address that works from here.

Without a token it falls back to the configured URL's host on the default port,
8081, so it still works before the instance is otherwise set up. That is a
guess: the CLI cannot see the server's `AURCACHE_REPO_PORT`, and the API URL's
port says nothing about the repository's, so against a server that moved it pass
`--port`. `--offline` forces
that fallback; `--port`, `--name` and `--siglevel` override the result outright.

For a signed repository, change the `SigLevel` it writes to `Required` once
the key is trusted; see [Signing the repository](#signing-the-repository).

`$arch` is pacman's own variable and is deliberately left unexpanded: one stanza
serves every architecture the instance builds for.

## Signing the repository

With `AURCACHE_SIGN=true`, the server signs:

- every package it publishes, with a detached `<package>.sig` beside it and the
  same signature in its `repo.db` entry (`%PGPSIG%`, as `repo-add
  --include-sigs` writes it);
- `repo.db` and `repo.files`, as `repo.db.sig` and `repo.files.sig` — under
  the repository's name, when `AURCACHE_REPO_NAME` gives it another. Renaming
  a signed repository renames these with its databases.

It signs with one OpenPGP key per instance, which it creates the first time it
starts with signing on, and prints the fingerprint of at every start:

```
Repository signing key fingerprint: 3F2A…
```

The key is Ed25519: a primary key that clients trust and that only certifies,
and a subkey under it that does the signing. Workers hold no key; the server
signs what they upload, as it publishes it.

**What a signature means.** "This AURCache instance published this file" — not
"this package is safe". AURCache builds whatever the PKGBUILD says, on any
worker you approved, and signs the result; approving workers and choosing
packages remain the decisions that matter. What signing protects against is
everything between the server and a client: a repository served over plain
HTTP, a proxy or mirror in the way, someone who can write to the served
directory without running the server, and a stale or swapped `repo.db`.

### Turning it on

1. Set `AURCACHE_SIGN=true` on the server and restart it.
2. On every client, trust the key, comparing the fingerprint with the one
   the server logs (and `GET /api/repo/signing` reports):

   ```bash
   curl -fsSL http://<server_ip>:8080/api/repo/key | sudo pacman-key --add -
   sudo pacman-key --lsign-key <fingerprint>
   ```

3. Once the server has signed what it published before (below:
   `backfilling` is `false`), set `SigLevel = Required` on the clients.

:::warning Trust the key on clients right away
pacman checks every signature it finds, even under `SigLevel = Optional`, the
stanza above included. From the moment the server signs, a client that does
not trust the key refuses the repository with `key "<fingerprint>" is
unknown`. That fingerprint is the signing subkey's, not the primary key's
the server logs: adding and trusting the key as
above takes care of both. Clients set to `SigLevel = Never` ignore signatures
and are not affected.
:::

Packages published before signing was turned on — or signed by another key —
are signed in the background after the server starts, a batch at a time,
without holding up publishing. Until that has finished, `GET /api/repo/info`
and `GET /api/repo/signing` say so (`backfilling`): a client requiring
signatures would refuse the packages not yet signed. Whatever cannot be
signed yet is retried later and reported in the **Logs** page. Some packages
the repository lists cannot be signed at all, and are reported there at every
attempt; rebuilding one publishes it again, signed:

- one no longer on disk, or not the file its entry describes, does not hold
  the rest up: no client can install it either;
- one clients can still install where signatures are optional — its entry
  has no checksum to check the file against, or it is a package the server
  cannot read, in a compression it does not support — keeps the repository
  `backfilling` until it is rebuilt, since a client requiring signatures
  refuses it.

### The key

It is kept as `signing-key.asc` in the CA directory (`AURCACHE_CA_DIR`,
`./data/ca` by default), readable by the server's user only. Back it up with
the CA: losing it means a new key, which every client has to trust again.

:::warning Keep the key out of the repository directory
A container whose only volume is the repository, as a 0.5 compose file has
it, keeps the CA inside the repository, as `repo/.ca` — and the signing key
with it. AURCache's own file server never serves it, but whatever else serves,
mirrors or syncs that directory hands out the key with the packages, and with
it the power to sign anything clients will install. The server warns at every
start when its key is inside the repository. Move it out: put the CA directory
on a volume of its own, which the server moves the CA to at its next start
([Hybrid image](../workers/hybrid.md#migrating)), or supply the key from
elsewhere with `AURCACHE_SIGNING_KEY_FILE`.
:::

- `AURCACHE_SIGNING_KEY_FILE` points at a key you supply instead — a Docker
  secret, a separate mount. The server reads it and never writes it. It may be
  armored or binary, and needs exactly one current signing subkey, or a
  primary key that signs: a key exported with `gpg --export-secret-keys`
  works, certifications by others included, and so does one exported with
  `gpg --export-secret-subkeys`, its primary key kept offline. Expired
  subkeys alongside are fine. A key with several current signing subkeys is
  refused: export only the one to sign with (`gpg --export-secret-subkeys
  <subkey fingerprint>!`, the `!` included). A server that does not sign
  says so when either variable is set. Prefer an Ed25519 key, as the server
  generates: an RSA key signs too, but through an RSA implementation with a
  known timing side channel
  ([RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071)),
  and the server warns about one.
- **Expiry.** A signing subkey stops signing when it expires or when its
  primary key does, whichever comes first: pacman takes every signature
  under an expired primary key for an expired one, the subkeys' included,
  and `gpg --quick-gen-key` gives a primary key three years by default. The
  server refuses to start with a key that stops signing within **30 days**,
  or whose primary key has expired or is revoked, and says when its key stops
  signing at every start. A server that runs on past that stops signing, and
  publishing fails until it has a key that signs. Extending the subkey or the
  primary key (`gpg --quick-set-expire`) changes the public key: have every
  client import the key again *before* the old expiry
  (`curl -fsSL …/api/repo/key | sudo pacman-key --add -`), then give the
  server the new key file and restart it — pacman checks a signature against
  the key as it imported it.
- Some unprotected Ed25519 keys GnuPG exports — about one in 256 — are
  refused with a message saying the library the server reads keys with
  refuses their checksum ([rPGP #901](https://github.com/rpgp/rpgp/issues/901)).
  A damaged key file is refused with the same message. Lock the key with a
  passphrase (`gpg --passwd <fingerprint>`), export it again and give the
  server the passphrase (below): the locked copy is read. If it is refused
  too, the file is damaged: restore the key from a backup.
- `AURCACHE_SIGNING_KEY_PASSPHRASE_FILE` names a file holding a passphrase:
  a key the server creates is locked with it, and a supplied one unlocked
  with it. It protects a copy of the key file — a backup, a volume snapshot —
  from whoever does not also have the passphrase file. It cannot protect the
  key from someone who can read both, which the server has to do to sign at
  all.
- A [dump](../cli/maintenance.md), even one taken with secrets, never carries the
  key: a client that trusts it installs whatever it signs, from wherever, long
  after the instance a dump came from is gone. Copy the file on purpose.
- The public key is served without authentication at `GET /api/repo/key`.
  Publishing it on a keyserver is your call, and cannot be undone:
  `curl -fsSL …/api/repo/key | gpg --import` then
  `gpg --keyserver keyserver.ubuntu.com --send-keys <fingerprint>`.

Rotating the key while the server runs is not supported yet. Replacing it — a
new file, or a supplied key in place of the generated one — has the server sign
everything again with the new key at its next start, and a client refuses
whatever is signed by a key it has not imported. So replace a key in this
order: first import and trust the new key on every client (`pacman-key --add`
and `--lsign-key`, as above, with the new key's file and fingerprint), then
restart the server with it. A new subkey under the same primary key needs the
same import, though the primary key's trust carries over.

Turning signing off removes the databases' signatures at the next start, since
the databases are written unsigned from then on. A package already published
keeps its signature, beside it and in `repo.db`, as long as it stays
published; publishing its package again — a new version, or a rebuild under
the same file name — publishes it unsigned and takes the old signature away
with the file it signed. pacman checks every signature it finds, even with
signatures optional, and those that stay verify only against the key. So a
client that trusts the key can go back to `SigLevel = Optional TrustAll`,
while one that never trusted it refuses each package still signed until that
package is published again: trust the key there, or set it to `SigLevel =
Never` meanwhile.

### Limits

- A database and its signature are two files, replaced one right after the
  other. A client that downloads `repo.db` while a build is being published can
  get the new database and the old signature; that one `pacman -Sy` fails, and
  the next succeeds. `repo-add` has the same window.
- Workers still install dependencies from this repository without checking
  signatures.
