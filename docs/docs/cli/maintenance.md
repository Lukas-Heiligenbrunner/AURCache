---
sidebar_position: 5
---

# Backup and restore

```bash
aurcli dump                              # authored state to a dated .tar.gz
aurcli dump --include-secrets -o backup.tar.gz
aurcli restore backup.tar.gz --dry-run
aurcli restore backup.tar.gz --on-existing skip
aurcli restore backup.tar.gz --clear     # replace: removes every package, setting and worker first
```

`--include-secrets` also exports the CA private key, worker certificates, API
token hashes, and the SSH key private package sources are cloned with — the file
becomes a credential and is written owner-readable only. Taking the dump's
secrets on restore replaces the CA, which invalidates every certificate the
server's workers hold.

A dump without secrets has nothing of SSH in it, and restores anywhere without
asking for a key. A dump with secrets carries what keeps private sources
cloning after a restore or a move to another machine, without adding a new key
to every repository:

- **The SSH key the server clones with**, generated or supplied through
  `AURCACHE_GIT_SSH_KEY`, exactly as it is stored. A key protected by a
  passphrase stays protected; the passphrase is never in a dump, so give the new
  server the same `AURCACHE_GIT_SSH_KEY_PASSPHRASE_FILE`.
- **The host keys trusted on first use** (`AURCACHE_SSH_DIR/known_hosts`). A
  file you pin with `AURCACHE_SSH_KNOWN_HOSTS` is yours, and is not carried.

What a restore does with them:

- **Everything is checked first, as it will be used.** A dump key that is to
  become the key in use has to open on this server: a protected key needs this
  server's `AURCACHE_GIT_SSH_KEY_PASSPHRASE_FILE` to hold its passphrase. If it
  does not open, or a file the restore would write cannot be written, the
  restore is refused whole, before any package is imported, and `--dry-run`
  says so (and nothing else). A file that cannot be replaced counts: a symlink,
  or a secret mounted at the server's own key file. Mount a key you supply
  somewhere else and name it with `AURCACHE_GIT_SSH_KEY`; the restore then
  leaves it alone.
- **Without `--secrets copy` or `--clear`, the SSH key is left alone.**
- **`--secrets copy`** puts the key in the server's own place,
  `AURCACHE_SSH_DIR/id_ed25519`, replacing the one a new server generated, and
  the server clones with it at once. The trusted host keys are merged into the
  server's own: none it recorded is lost, and a dump that trusts a different
  key of the same type for a host this server already knows is refused. A key
  in an older (PEM) format is refused too, since it cannot be checked before it
  is written: convert it with `ssh-keygen -p` and take the dump again. A
  server with `AURCACHE_GIT_SSH_KEY_PASSPHRASE_FILE` protects an unprotected
  dump key with that passphrase before writing it.
- **A key the restore replaces is kept**, beside it and readable by the server's
  user only, named for its fingerprint: `id_ed25519.replaced-SHA256-…`, with
  the fingerprint's `/` and `+` written `_` and `-`. To go back to it, move it
  over `id_ed25519` and restart the server. The same key is never written
  twice: one already in place, however it is protected, is left as it is.
- **`--clear` takes what the dump carries.** The key and the trusted host keys
  are the dump's when it carries them; the host keys this server had are kept
  beside the file, as `known_hosts.replaced-sha256-…`, as a replaced key is. A
  dump without them leaves this server's key and trusted hosts as they are, as
  it leaves the CA: nothing has to be given a new key.
- **What you configured is never written.** A key named by
  `AURCACHE_GIT_SSH_KEY` stays the key in use, even where it is the server's own
  key file, and the activity log says so when it differs from the dump's. A
  dump whose key is that key, as one taken on the same server is, writes no
  copy of it anywhere. A pinned `AURCACHE_SSH_KNOWN_HOSTS` is left alone.

Values set on workers from AURCache are included in a backup with the worker
they belong to, and restored with it.

`aurcli raw` calls an arbitrary API path for endpoints without a dedicated
subcommand:

```bash
aurcli raw GET /packages/list
aurcli raw POST /workers/3/approve
```
