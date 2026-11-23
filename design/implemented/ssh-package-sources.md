# Design: SSH package sources

Status: **Implemented** · Last updated: 2026-11-23

The server clones a package's own git repository to read its PKGBUILD, and
until now only over http(s), so a private packaging repository could not be
added. What an operator sees is in
[the user docs](../../docs/docs/Configuration/ssh-sources.md); this is what a
maintainer needs to keep in mind.

## Two keys, two remotes

The server's key (`aurcache_utils::ssh`) is only for a package's own
repository, and only `SnapshotStore` offers it (`git::Access::Server`). The VCS
check asks the remotes that a PKGBUILD *names*, which its author chose, so it
uses `Access::Anonymous` and an SSH remote is refused there before
connecting. Those sources belong to the workers' keys
(`docs/docs/workers/credentials.md`).

## libssh2 through git2

`git2`'s `ssh` feature, rather than shelling out to `git`: the server image has
neither `git` nor `ssh`, and clone, fetch and `ls-remote` stay one code path
that differs only in its callbacks. The cost: no `ssh_config`, no agent, and
libssh2 picks the host-key type.

Everything rests on libgit2 calling our callbacks:

- **The backend.** Only libssh2 calls them; libgit2's `exec` backend runs
  `ssh` and calls neither. A native install links the system libgit2, so
  `git::ssh_unsupported` asks `git_libgit2_feature_backend` at start and, for
  anything but libssh2, the server has no key and SSH sources are refused.
- **The port.** The certificate callback is told the host but not the port, so
  a key is judged under a host name (`[host]:port`) that comes from our
  reading of the URL, and that reading must be libgit2's. libgit2 also reaches any URL
  with a colon in it over SSH. `source::transport` is therefore an allowlist
  of forms whose reading is unambiguous (the account and host admit no
  brackets, `%`, `@` or `:`); anything else is refused before connecting.
- **No route without a check.** A non-SSH route still installs a certificate
  callback, which refuses any SSH host key, so a URL reached over SSH against
  our reading fails closed. On an SSH route the callback refuses a host other
  than the one parsed.
- libgit2 replaces a callback's error with a generic one; `git::Refusal` keeps
  the reason and reports it instead.

## The key and the host keys

The key is `AURCACHE_GIT_SSH_KEY`, never generated or replaced, or an ed25519
key generated (with `ssh-key`) in `AURCACHE_SSH_DIR`, a state directory handled
like the CA's (`HybridDir::Ssh`, `private-state.sh`, the parse sandbox's private
paths). A key that cannot be loaded leaves the server running, with the reason
reported for each SSH source and in `GET /api/ssh`. A dump carries the CA but
not this key, so a restore onto a new machine starts with a new key.

Host keys are trusted on first use into `AURCACHE_SSH_DIR/known_hosts`, which
OpenSSH calls `accept-new` and workers do too, with an `ssh.host_trusted`
activity event for the one fingerprint that nothing checked; or pinned to the
operator's `AURCACHE_SSH_KNOWN_HOSTS`, never written. Entries match by plain or
hashed name only: the server writes plain ones and `ssh-keyscan -H` hashed
ones, and a pattern that it does not read vouches for nothing.

Built-in keys for the big forges were rejected: they rotate, and a stale key in
a release fails closed everywhere until an upgrade.
