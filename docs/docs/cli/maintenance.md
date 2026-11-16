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

Taking the secrets on restore puts the key in the server's own place,
`AURCACHE_SSH_DIR/id_ed25519`, replacing the one a new server generated, and the
server clones with it at once. If the target sets `AURCACHE_GIT_SSH_KEY`, that
key stays the one in use and is never written to; the activity log says so when
it differs from the dump's. The trusted host keys replace the server's own, and
a pinned `AURCACHE_SSH_KNOWN_HOSTS` is left alone.

Values set on workers from AURCache are included in a backup with the worker
they belong to, and restored with it.

`aurcli raw` calls an arbitrary API path for endpoints without a dedicated
subcommand:

```bash
aurcli raw GET /packages/list
aurcli raw POST /workers/3/approve
```
