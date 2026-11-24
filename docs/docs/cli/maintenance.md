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
token hashes, the repository's [signing
key](../Configuration/pacman-repo.md#the-key) file as stored (still locked, if
it is locked; the passphrase is never in a dump), and the server's own [SSH
key and `known_hosts`](../Configuration/ssh-sources.md) — the file becomes a
credential and is written owner-readable only. Taking the dump's secrets on
restore (`--secrets copy`, or `--clear`) replaces the CA, which invalidates
every certificate the server's workers hold.

It also takes the dump's signing key, which is checked first: if it does not
unlock with this server's passphrase (`AURCACHE_SIGNING_KEY_PASSPHRASE_FILE`),
has expired or cannot sign, nothing is imported. The restore's report says,
when it starts, what it does with the key:

- `will be replaced`: written over `signing-key.asc` once the packages are
  imported, the old file kept beside it as `signing-key.asc.replaced-<hash>`;
  a failure to write it is reported with the rest of the restore's progress.
  The server signs with the restored key from its next start, and re-signs
  what the old key signed then. Restart it, then have every client trust the
  key as in [Signing the
  repository](../Configuration/pacman-repo.md#signing-the-repository)
  (`pacman-key --add` and `--lsign-key`), unless they already trust it.
- `unchanged`: this server's key file is already the dump's.
- `kept (supplied)`: the server signs with `AURCACHE_SIGNING_KEY_FILE`, which a
  restore never writes.

It takes the dump's SSH key and `known_hosts` the same way. The key is checked
first as well: if it is not an OpenSSH private key, or has a passphrase,
nothing is imported. The report gives a line for each, with the same three
answers, and a replaced file is kept beside the new one as
`<file>.replaced-<hash>`. Clones use both at once; the Settings page and
`aurcli ssh key` show the restored key from the server's next start.
`known_hosts` is replaced, not merged: a host that only this server's file
listed is trusted on first use again, which the activity log records. A file
supplied with `AURCACHE_GIT_SSH_KEY` or `AURCACHE_SSH_KNOWN_HOSTS` stays out of
the dump, and a restore never writes a supplied file.

A dump without one of these files leaves the server's as it is. Mount the
directory (`AURCACHE_CA_DIR`, `AURCACHE_SSH_DIR`), not a file alone: a file
bind-mounted on its own cannot be replaced, and the restore reports that as a
failure.

Values set on workers from AURCache are included in a backup with the worker
they belong to, and restored with it.

`aurcli raw` calls an arbitrary API path for endpoints without a dedicated
subcommand:

```bash
aurcli raw GET /packages/list
aurcli raw POST /workers/3/approve
```
