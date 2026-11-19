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

It also asks whether the repository is signed, which the server answers
without a token too (`GET /api/repo/signing`). If it is, the stanza says
`SigLevel = Required`, the commands to trust the key are printed with it, and
`--install` first offers to run them: it shows the key's fingerprint and asks
before adding the key to pacman's keyring and locally signing it (through
`sudo` unless run as root). Answering no, or running without a terminal to ask
on, leaves the keyring alone, prints the commands, and does not write the
section either — pacman would refuse it, even with signatures optional, until
the key is trusted. Run it again once the key is trusted, or pass `--siglevel`
to write the section regardless. A section already in `pacman.conf` is left as
it is, its `SigLevel` included: change that one yourself.

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
2. On every client, trust the key: `aurcli repo config --install`, or by hand,
   comparing the fingerprint with the one the server logs or the Settings page
   shows:

   ```bash
   curl -fsSL http://<server_ip>:8080/api/repo/key | sudo pacman-key --add -
   sudo pacman-key --lsign-key <fingerprint>
   ```

3. Once the server has signed what it published before (below), set
   `SigLevel = Required` on the clients.

:::warning Trust the key on clients right away
pacman checks every signature it finds, even under `SigLevel = Optional`, the
stanza above included. From the moment the server signs, a client that does
not trust the key refuses the repository with `key "<fingerprint>" is
unknown`. That fingerprint is the signing subkey's, not the primary key's
the server logs and the Settings page shows: adding and trusting the key as
above takes care of both. Clients set to `SigLevel = Never` ignore signatures
and are not affected.
:::

Packages published before signing was turned on — or signed by another key —
are signed in the background after the server starts, a batch at a time,
without holding up publishing. Until that has finished, the Settings page and
`GET /api/repo/info` say so (`signing.backfilling`), and `aurcli repo config`
keeps `SigLevel = Optional TrustAll`: a client requiring signatures would
refuse the packages not yet signed. Whatever cannot be signed yet is retried
later and reported in the **Logs** page. A package the repository lists but
no longer has on disk is reported there once and does not hold the rest up:
no client can download it either, and rebuilding it publishes it again.

### The key

It is kept as `signing-key.asc` in the CA directory (`AURCACHE_CA_DIR`,
`./data/ca` by default), readable by the server's user only. Back it up with
the CA: losing it means a new key, which every client has to trust again.

- `AURCACHE_SIGNING_KEY_FILE` points at a key you supply instead — a Docker
  secret, a separate mount. The server reads it and never writes it. It may be
  armored or binary, and needs a primary key or subkey that signs; a key
  exported with `gpg --export-secret-keys` works, certifications by others
  included, and so does one exported with `gpg --export-secret-subkeys`, its
  primary key kept offline. A server that does not sign says so when either
  variable is set. Its newest valid signing subkey signs, as GnuPG picks, and
  the server refuses to start with a key where nothing signs. Prefer an
  Ed25519 key, as the server generates: an RSA key signs too, but through an
  RSA implementation with a known timing side channel
  ([RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071)),
  and the server warns about one.
- **Subkeys expire, and so does the primary key.** A signing subkey stops
  signing when it expires or when its primary key does, whichever comes
  first: pacman takes every signature under an expired primary key for an
  expired one, the subkeys' included, and `gpg --quick-gen-key` gives a
  primary key three years by default. The server refuses to start with a key
  whose primary key has expired or is revoked. From 30 days before the
  signing subkey stops signing, the server says so at start and once a day
  after. A day before, the server moves on to the next valid signing subkey,
  if the key has one that signs for longer, and signs everything again with
  it; without one — and there is none when it is the primary key that
  expires — nothing signs once it has, and publishing fails until the server
  has a key that does. Adding a subkey or extending one or the primary key
  (`gpg --quick-set-expire`) changes the public key: give the server the new
  key file and restart it, and have every client import the key again before
  the old expiry (`curl -fsSL …/api/repo/key | sudo pacman-key --add -`) —
  pacman checks a signature against the key as it imported it, and refuses
  one by a subkey it has never seen.
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

Replacing the key — a new file, or a supplied key in place of the generated
one — has the server sign everything again with the new key at its next start.
Every client has to trust the new key before then, or it refuses the
repository until it does.

Turning signing off removes the databases' signatures at the next start, since
the databases are written unsigned from then on. Packages keep the signatures
they have, beside them and in `repo.db`, until they are rebuilt — and pacman
checks every signature it finds, even with signatures optional. So a client
that trusts the key can go back to `SigLevel = Optional TrustAll`, while one
that never trusted it keeps refusing those packages until they are rebuilt:
trust the key there, or set it to `SigLevel = Never` meanwhile.

### Limits

- A database and its signature are two files, replaced one right after the
  other. A client that downloads `repo.db` while a build is being published can
  get the new database and the old signature; that one `pacman -Sy` fails, and
  the next succeeds. `repo-add` has the same window.
- Workers still install dependencies from this repository without checking
  signatures.
