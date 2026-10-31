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

`--include-secrets` also exports the CA private key, worker certificates and
API token hashes — the file becomes a credential and is written owner-readable
only. Taking the dump's secrets on restore replaces the CA, which invalidates
every certificate the server's workers hold.

Values set on workers from AURCache are included in a backup with the worker
they belong to, and restored with it.

`aurcli raw` calls an arbitrary API path for endpoints without a dedicated
subcommand:

```bash
aurcli raw GET /packages/list
aurcli raw POST /worker/approve --body '{"id": 3}'
```
