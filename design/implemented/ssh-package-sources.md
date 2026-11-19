# Design: SSH package sources

Status: **Implemented** · Last updated: 2026-11-16

A package can be added from a git repository, and the server clones that
repository itself, to read the PKGBUILD before anything is built. Until now
that clone only worked over http(s): `git2` was built without an SSH
transport, and the server had no key. A private repository is usually only
reachable over SSH (`git@host:repo.git`, `ssh://git@host:2222/repo.git`), so
private packaging repositories could not be built at all.

Operator-facing documentation: `docs/docs/Configuration/ssh-sources.md`.

## Roles

Two different keys answer to two different remotes, and stay apart:

| Key | Held by | Used for |
|---|---|---|
| The server's (this design) | the server | the package's **own** repository: clone, fetch, `ls-remote` |
| A worker's (`WORKER_GIT_SSH_KEY`, `docs/workers/credentials.md`) | each worker | the sources a PKGBUILD's `source=()` downloads while building |

The server offers its key only through `SnapshotStore`, which is what clones a
package's own source. The VCS check (`vcs_check.rs`), which asks the remotes a
PKGBUILD *names* whether they moved, uses `Access::Anonymous`: those remotes are
written by whoever wrote the PKGBUILD, and the server's key is not theirs to be
offered. An SSH source there is refused before connecting, and reported as
unresolved, as it was before (it then failed for want of a transport).

## Transport: libssh2 through `git2`, not the `git` CLI

`git2`'s `ssh` feature (libssh2) was chosen over shelling out to `git` with
`GIT_SSH_COMMAND`:

- **No new runtime dependency.** The server image is `debian:bookworm-slim`
  with neither `git` nor `openssh-client`; libssh2 is vendored and linked
  statically like the vendored OpenSSL already is. The Arch package links the
  system `libgit2`, which Arch builds against libssh2 already. A libgit2 built
  otherwise -- without SSH, or with its `exec` backend, which runs `ssh` and
  calls none of the callbacks -- is found at start
  (`git_libgit2_feature_backend`, `git::SshBackend`), and SSH sources are then
  refused with that reason in place of the key, rather than cloned past the
  server's key and host-key checks.
- **One code path.** Clone, fetch and `ls-remote` stay `git2` calls for every
  remote; the only difference for SSH is the callbacks. Shelling out would have
  meant a second implementation of the network half for one scheme, with its
  own error handling and its own idea of refs.
- **Host keys are checked by our code**, in the certificate callback, against
  files we read. The failure messages can then name the host, both fingerprints
  and the command that fixes it, which OpenSSH's own messages would not (they
  would also have to be scraped from stderr).

What it costs, and is documented:

- `ssh_config` is not read: no `Host` aliases, `ProxyJump` or `IdentityFile`.
  The URL carries the real host and port.
- libssh2 picks the host-key algorithm (ECDSA before ed25519 before RSA), not
  what `known_hosts` already holds as OpenSSH would. A host recorded only by
  another key type is refused (`HostKeyCheck::OtherTypes`), saying which types
  are recorded and how to record the offered one, not warning of
  interception; the docs say to record every type (`ssh-keyscan` without
  `-t`). Only the same type with another key reads as a changed key.
- No ssh-agent and no hardware keys. A passphrase only from a file (see below).
- libgit2 replaces a callback's error message with a generic one ("invalid or
  unknown remote ssh hostkey"). `git.rs` keeps the callback's reason beside it
  (`Refusal`) and reports that instead.

## The key

`aurcache_utils::ssh`, in precedence order:

1. `AURCACHE_GIT_SSH_KEY` names a key: used as is, never generated or
   overwritten. A missing file is an error at start, not a reason to generate
   one: two keys would make "which one is in use?" ambiguous exactly when an
   authentication failure is being debugged (the worker's rule too).
2. Otherwise an ed25519 key is generated on first start, in Rust (`ssh-key`
   crate; the image has no `ssh-keygen`), at `AURCACHE_SSH_DIR/id_ed25519`
   (`./data/ssh`), written `0600` in a `0700` directory, its `.pub` beside it.

A key that cannot be loaded does not stop the server: everything else works,
and each SSH source fails with the reason (`SshKey::Unavailable`).

Either key may be protected by a passphrase, given in a file named by
`AURCACHE_GIT_SSH_KEY_PASSPHRASE_FILE`. It is read once at start, held in a
`Zeroizing` string with a redacting `Debug`, and passed to libssh2 as the
fourth argument of `Cred::ssh_key`. For an OpenSSH-format key the server checks
at start that it decrypts the key (`ssh-key`'s `encryption` feature), so a
wrong passphrase is the stated reason rather than an authentication failure
later; an older format is left to libssh2. The generated key is written
unprotected: a passphrase the server chose and stored beside it would protect
nothing. The operator can protect it afterwards with `ssh-keygen -p`.

The public half is logged at start, returned by `GET /api/ssh`, shown on the
settings page and in the add dialog once the entry is an SSH remote, and
printed by `aurcli ssh key` (the key alone on the first line).

`SshInfo` holds a list of named keys (public key, fingerprint, origin) and the
host-key policy. There is one key today, named `default`. It is a list so that
keys chosen per repository (see "Not covered") can join it without the
response changing shape, and `aurcli ssh key [NAME]` is a subcommand of an
`ssh` group for the same reason.

### Keeping it

`AURCACHE_SSH_DIR` is a state directory like the CA's, with the same handling:

- `HybridDir::Ssh`: in a container whose `./data/ssh` is not on a volume, the
  key lives in `repo/.ssh`, and moves out once a volume appears. A key lost
  with a container would be silently replaced by one no remote knows.
- `private-state.sh` closes it (`0700`) at every container start.
- It is on the startup warning's list of unpersisted directories.

### Keeping it from PKGBUILDs

`pkgbuild::private_state` names `AURCACHE_SSH_DIR`, the key
`AURCACHE_GIT_SSH_KEY` points at and the passphrase file, so a PKGBUILD parse
cannot read any of them. On a
kernel without Landlock the parse sandbox checks file modes instead and refuses
to parse while the key is readable by other users -- a world-readable Docker
secret then stops every parse, with a message saying which file. That is the
sandbox doing its job (the parse user *could* read it), and the docs say to keep
the key `0600`.

Nothing returns or logs the private key: the API type has no field for it, and
a test asserts neither the JSON nor `Debug` of `SshAccess` contains it.

## Host keys

Never trusted silently. Two policies (`HostKeyPolicy`):

- **Trust on first use** (default). The first connection to a host records its
  key in `AURCACHE_SSH_DIR/known_hosts`, and an `ssh.host_trusted` warning in
  the activity log with the name, key type and fingerprint: the one key nothing
  checked, recorded where someone will see it to check it. A different key
  from that host later is refused.

  Every key the file in use vouches for is listed with its fingerprint in
  `SshInfo::trusted_hosts` (`TrustedHosts`, a list or why the file cannot be
  read), on the settings page and by `aurcli ssh hosts`, so a fingerprint can
  be compared with the one its host publishes at any time, not only in the
  log entry. A hashed entry is listed without a name, since it has none to
  give; `@revoked` and `@cert-authority` lines vouch for nothing and are not
  listed. This is
  OpenSSH's `StrictHostKeyChecking=accept-new`, which is also what workers do
  without a `known_hosts`, so the two behave alike.
- **Pinned**: `AURCACHE_SSH_KNOWN_HOSTS` names an operator's file. Only its
  hosts are trusted, it is never written to, and an unknown host is refused
  with the `ssh-keyscan` line that would add it.

Hosts are named as OpenSSH names them (`host`, or `[host]:port` off port 22),
from the URL rather than from libgit2. The certificate callback is told the
host but not the port, so the URL has to be read exactly as libgit2 reads it,
or a key could be judged under one port's name while another is connected to.
`aurcache_common::source` mirrors libgit2 1.9: the scp-like form through a
copy of `git_net_url_parse_scp`'s state machine, brackets included
(`[git@host:2222]:repo` connects to port 2222), and the URL untrimmed. The
callback still refuses a host other than the one parsed, which catches what a
divergence in the host alone would cause.

libgit2 also decides *whether* a URL is reached over SSH: by prefix, ignoring
case, and for anything else with a colon in it, including `./a:b` and schemes
it has no transport for. `source::transport` makes the same decision. What it
would reach over SSH with no host this can name (`Transport::UnreadableSsh`)
is refused before connecting, with either `Access`: without our callbacks,
libgit2 would connect checking only its own idea of `~/.ssh/known_hosts`.

The file is parsed with `ssh-key`;
hashed names (`|1|salt|hash`, HMAC-SHA1), comma lists, `*`/`?` globs and `!`
negations match as in OpenSSH. `@revoked` refuses the key wherever it appears;
`@cert-authority` lines are ignored, since certificates are not checked. A line
that does not parse is skipped with a warning, as OpenSSH skips it.

### Rejected: built-in keys for GitHub, GitLab, Codeberg

Pinning the big forges' published keys in the binary would remove first-use
trust for the common case. It was left out: those keys rotate (GitHub's RSA key
did in 2023), and a release carrying a stale key fails closed for every
deployment until it is upgraded. An operator who wants pinning has
`AURCACHE_SSH_KNOWN_HOSTS`, filled by one `ssh-keyscan`.

## Not covered

- A dump with secrets carries the worker CA, not the SSH key. Restoring onto a
  new machine therefore generates a new key, which has to be added to the
  remotes again (or `AURCACHE_GIT_SSH_KEY` points at the old one).
- The VCS check does not resolve SSH sources a PKGBUILD names (see Roles).
